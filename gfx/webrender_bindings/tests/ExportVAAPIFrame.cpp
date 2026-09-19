/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/hwcontext_vaapi.h>

#include "va_drmcommon.h"
}

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <string>

struct Decoder {
  int accessLock = -1;
  AVBufferRef* device = nullptr;
  AVCodecContext* context = nullptr;
  AVFrame* frame = av_frame_alloc();
  AVFrame* mapped = av_frame_alloc();
  AVFrame* reference = av_frame_alloc();
  AVPacket* packet = av_packet_alloc();
  ~Decoder() {
    if (accessLock >= 0) close(accessLock);
    av_frame_free(&mapped);
    av_frame_free(&reference);
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&context);
    av_buffer_unref(&device);
  }
};

struct Export {
  VADRMPRIMESurfaceDescriptor descriptor{};
  bool valid = false;
  ~Export() {
    for (uint32_t i = 0; valid && i < descriptor.num_objects && i < 4; ++i) {
      close(descriptor.objects[i].fd);
    }
  }
};

static bool Check(int aResult, const char* aOperation) {
  if (aResult >= 0) return true;
  char error[AV_ERROR_MAX_STRING_SIZE];
  av_strerror(aResult, error, sizeof(error));
  std::fprintf(stderr, "%s: %s\n", aOperation, error);
  return false;
}

static bool SetNumber(const char* aName, uint64_t aValue) {
  return setenv(aName, std::to_string(aValue).c_str(), 1) == 0;
}

static uint32_t Read32(const uint8_t* aBytes) {
  return uint32_t(aBytes[0]) | (uint32_t(aBytes[1]) << 8) |
         (uint32_t(aBytes[2]) << 16) | (uint32_t(aBytes[3]) << 24);
}

int main(int argc, char** argv) {
  if (argc != 5 && argc != 6) {
    std::fprintf(stderr,
                 "Usage: %s DRM_RENDER_NODE VP9_IVF RUST_TEST_BINARY "
                 "OUTPUT_DIR [TEST_FILTER]\n",
                 argv[0]);
    return 1;
  }
  if ((avcodec_version() >> 16) != 60 || (avutil_version() >> 16) != 58) {
    std::fprintf(stderr,
                 "This fixture requires FFmpeg libavcodec 60/libavutil 58\n");
    return 1;
  }
  struct stat node{};
  if (stat(argv[1], &node) || !S_ISCHR(node.st_mode)) return 1;
  std::ifstream input(argv[2], std::ios::binary);
  uint8_t header[32], packetHeader[12];
  if (!input.read(reinterpret_cast<char*>(header), sizeof(header)) ||
      std::memcmp(header, "DKIF", 4) || header[4] || header[5] ||
      header[6] != 32 || header[7] || std::memcmp(header + 8, "VP90", 4) ||
      !input.read(reinterpret_cast<char*>(packetHeader),
                  sizeof(packetHeader))) {
    std::fprintf(stderr, "Expected a VP9 IVF file\n");
    return 1;
  }
  const uint32_t packetSize = Read32(packetHeader);
  if (!packetSize || packetSize > 16 * 1024 * 1024) return 1;
  Decoder decoder;
  const AVCodec* codec = avcodec_find_decoder(AV_CODEC_ID_VP9);
  if (!codec || !decoder.frame || !decoder.mapped || !decoder.reference ||
      !decoder.packet)
    return 1;
  decoder.context = avcodec_alloc_context3(codec);
  if (!decoder.context ||
      !Check(av_hwdevice_ctx_create(&decoder.device, AV_HWDEVICE_TYPE_VAAPI,
                                    argv[1], nullptr, 0),
             "VA-API device"))
    return 1;
  decoder.context->hw_device_ctx = av_buffer_ref(decoder.device);
  if (!decoder.context->hw_device_ctx) return 1;
  decoder.context->thread_count = 1;
  decoder.context->get_format = [](AVCodecContext*,
                                   const AVPixelFormat* aFormats) {
    for (; *aFormats != AV_PIX_FMT_NONE; ++aFormats) {
      if (*aFormats == AV_PIX_FMT_VAAPI) return *aFormats;
    }
    return AV_PIX_FMT_NONE;
  };
  if (!Check(avcodec_open2(decoder.context, codec, nullptr),
             "Open VP9 decoder") ||
      !Check(av_new_packet(decoder.packet, packetSize), "Allocate packet") ||
      !input.read(reinterpret_cast<char*>(decoder.packet->data), packetSize) ||
      !Check(avcodec_send_packet(decoder.context, decoder.packet),
             "Send VP9 packet"))
    return 1;
  int received = avcodec_receive_frame(decoder.context, decoder.frame);
  if (received == AVERROR(EAGAIN)) {
    if (!Check(avcodec_send_packet(decoder.context, nullptr),
               "Flush VP9 decoder"))
      return 1;
    received = avcodec_receive_frame(decoder.context, decoder.frame);
  }
  if (!Check(received, "Decode VA-API frame") ||
      decoder.frame->format != AV_PIX_FMT_VAAPI ||
      !decoder.frame->hw_frames_ctx)
    return 1;
  auto* device = reinterpret_cast<AVHWDeviceContext*>(decoder.device->data);
  auto* vaapi = static_cast<AVVAAPIDeviceContext*>(device->hwctx);
  Export exported;
  if (vaExportSurfaceHandle(
          vaapi->display, VASurfaceID(uintptr_t(decoder.frame->data[3])),
          VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
          VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS,
          &exported.descriptor) != VA_STATUS_SUCCESS)
    return 1;
  exported.valid = true;
  if (vaSyncSurface(vaapi->display,
                    VASurfaceID(uintptr_t(decoder.frame->data[3]))) !=
      VA_STATUS_SUCCESS) {
    std::fprintf(stderr, "VA-API producer did not complete\n");
    return 1;
  }
  decoder.mapped->format = AV_PIX_FMT_DRM_PRIME;
  if (!Check(av_hwframe_map(decoder.mapped, decoder.frame,
                            AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT),
             "Export DRM PRIME") ||
      !Check(av_hwframe_transfer_data(decoder.reference, decoder.frame, 0),
             "Read reference planes") ||
      decoder.reference->format != AV_PIX_FMT_NV12)
    return 1;
  const auto* drm =
      reinterpret_cast<const AVDRMFrameDescriptor*>(decoder.mapped->data[0]);
  const auto* allocation = &exported.descriptor;
  constexpr uint32_t kR8 = 0x20203852;
  constexpr uint32_t kGR88 = 0x38385247;
  if (!drm || drm->nb_objects != 1 || drm->nb_layers != 2 ||
      drm->layers[0].format != kR8 || drm->layers[1].format != kGR88) {
    std::fprintf(stderr, "Fixture requires one-object, separate-layer NV12\n");
    return 1;
  }
  for (const auto& layer : drm->layers) {
    if (&layer >= drm->layers + drm->nb_layers) break;
    if (layer.nb_planes != 1 || layer.planes[0].object_index != 0) return 1;
  }
  const int fd = drm->objects[0].fd;
  decoder.accessLock = memfd_create("wr-video-test-access", MFD_ALLOW_SEALING);
  if (decoder.accessLock < 0 ||
      ftruncate(decoder.accessLock, sizeof(uint32_t)) ||
      fcntl(decoder.accessLock, F_ADD_SEALS,
            F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL) < 0)
    return 1;
  const int flags = fcntl(fd, F_GETFD);
  if (flags < 0 || fcntl(fd, F_SETFD, flags & ~FD_CLOEXEC) < 0) return 1;
  std::error_code error;
  std::filesystem::create_directories(argv[4], error);
  if (error) return 1;
  const auto referencePath = std::filesystem::absolute(
      std::filesystem::path(argv[4]) / "reference.nv12");
  std::ofstream reference(referencePath, std::ios::binary);
  for (int plane = 0; plane < 2; ++plane) {
    for (int y = 0; y < decoder.frame->height >> plane; ++y) {
      reference.write(
          reinterpret_cast<char*>(decoder.reference->data[plane] +
                                  y * decoder.reference->linesize[plane]),
          decoder.frame->width);
    }
  }
  reference.close();
  if (!reference || setenv("WR_NV12_REFERENCE", referencePath.c_str(), 1) ||
      !SetNumber("WR_NV12_FD", fd) ||
      !SetNumber("WR_NV12_ACCESS_LOCK_FD", decoder.accessLock) ||
      !SetNumber("WR_NV12_BYTES", drm->objects[0].size) ||
      !SetNumber("WR_NV12_MODIFIER", drm->objects[0].format_modifier) ||
      !SetNumber("WR_NV12_WIDTH", decoder.frame->width) ||
      !SetNumber("WR_NV12_HEIGHT", decoder.frame->height) ||
      !SetNumber("WR_NV12_ALLOC_WIDTH", allocation->width) ||
      !SetNumber("WR_NV12_ALLOC_HEIGHT", allocation->height) ||
      !SetNumber("WR_NV12_Y_OFFSET", drm->layers[0].planes[0].offset) ||
      !SetNumber("WR_NV12_UV_OFFSET", drm->layers[1].planes[0].offset) ||
      !SetNumber("WR_NV12_Y_PITCH", drm->layers[0].planes[0].pitch) ||
      !SetNumber("WR_NV12_UV_PITCH", drm->layers[1].planes[0].pitch) ||
      !SetNumber("WR_NV12_DRM_MAJOR", major(node.st_rdev)) ||
      !SetNumber("WR_NV12_DRM_MINOR", minor(node.st_rdev)))
    return 1;
  std::printf(
      "VA-API frame %dx%d, allocation %ux%u, modifier 0x%llx, %zu bytes\n",
      decoder.frame->width, decoder.frame->height, allocation->width,
      allocation->height,
      static_cast<unsigned long long>(drm->objects[0].format_modifier),
      drm->objects[0].size);
  std::fflush(stdout);
  const pid_t child = fork();
  if (child < 0) return 1;
  if (!child) {
    const char* filter = argc == 6 ? argv[5] : "vaapi_nv12";
    execl(argv[3], argv[3], filter, "--ignored", "--nocapture",
          "--test-threads=1", nullptr);
    std::perror("exec Rust tests");
    _exit(1);
  }
  int status;
  while (waitpid(child, &status, 0) < 0) {
    if (errno != EINTR) return 1;
  }
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
