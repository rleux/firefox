/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <fcntl.h>
#include <gbm.h>
#include <unistd.h>
#ifdef XP_LINUX
#  include <sys/eventfd.h>
#  include <sys/syscall.h>

#  include <algorithm>
#  include <chrono>
#  include <cmath>
#  include <future>
#  include <string>

#  include "GLBlitHelper.h"
#  include "GLContext.h"
#  include "GLContextProvider.h"
#  include "base/linux_memfd_defs.h"
#  include "mozilla/ScopeExit.h"
#  include "mozilla/gfx/gfxVars.h"
#  include "mozilla/webgpu/SharedTextureDMABuf.h"
#  include "mozilla/webrender/RenderCompositorVulkan.h"
#  include "mozilla/webrender/RenderDMABUFTextureHost.h"
#endif

#include "gtest/gtest.h"
#include "mozilla/NotNull.h"
#include "mozilla/gfx/FileHandleWrapper.h"
#include "mozilla/gfx/GraphicsMessages.h"
#include "mozilla/ipc/FileDescriptor.h"
#include "mozilla/layers/LayersSurfaces.h"
#include "mozilla/widget/DMABufSurface.h"
#include "nsTArray.h"

using namespace mozilla;
using namespace mozilla::gfx;
using namespace mozilla::layers;

// These tests verify that DMABufSurface descriptors and surfaces round-trip
// stably through serialization and import.

static RefPtr<FileHandleWrapper> MakeFd() {
  int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
  if (fd == -1) return nullptr;
  return new FileHandleWrapper(UniqueFileHandle(fd));
}

// Matches what DMABufSurfaceRGBA::Serialize() produces for a single-plane
// 128×128 RGBA surface.
static SurfaceDescriptor MakeRGBADescriptor(RefPtr<FileHandleWrapper> fd) {
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 4> fds;
  fds.AppendElement(WrapNotNull(fd));
  AutoTArray<uint64_t, 1> modifiers = {0};
  AutoTArray<uint32_t, 4> width = {128}, height = {128}, strides = {512},
                          offsets = {0};
  AutoTArray<uint32_t, 1>
      format;  // intentionally empty; Serialize() leaves this unset for RGBA
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 1> fence;
  AutoTArray<ipc::FileDescriptor, 1> refCount;
  return SurfaceDescriptor(SurfaceDescriptorDMABuf(
      DMABufSurface::SURFACE_RGBA, GBM_FORMAT_ARGB8888, modifiers, 0, fds,
      width, height, width, height, format, strides, offsets,
      gfx::YUVColorSpace::BT601, gfx::ColorRange::LIMITED,
      gfx::ColorSpace2::UNKNOWN, gfx::TransferFunction::Default, 0, fence, 1, 0,
      refCount, nullptr, false, gfx::HDRMetadata(), Nothing(), Nothing(),
      Nothing()));
}

// Matches what DMABufSurfaceYUV::Serialize() produces for a two-plane 128×128
// (NV12-style, 4:2:0) YUV surface.
static SurfaceDescriptor MakeYUVDescriptor(RefPtr<FileHandleWrapper> fd0,
                                           RefPtr<FileHandleWrapper> fd1) {
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 4> fds;
  fds.AppendElement(WrapNotNull(fd0));
  fds.AppendElement(WrapNotNull(fd1));
  AutoTArray<uint64_t, 4> modifiers = {0, 0};
  // Y plane full size, UV plane half size (4:2:0 chroma).
  AutoTArray<uint32_t, 4> width = {128, 64}, height = {128, 64},
                          widthAligned = {128, 64}, heightAligned = {128, 64},
                          format = {0, 0}, strides = {128, 128},
                          offsets = {0, 0};
  AutoTArray<NotNull<RefPtr<FileHandleWrapper>>, 1> fence;
  AutoTArray<ipc::FileDescriptor, 1> refCount;
  return SurfaceDescriptor(SurfaceDescriptorDMABuf(
      DMABufSurface::SURFACE_YUV, VA_FOURCC_NV12, modifiers, 0, fds, width,
      height, widthAligned, heightAligned, format, strides, offsets,
      gfx::YUVColorSpace::BT601, gfx::ColorRange::LIMITED,
      gfx::ColorSpace2::UNKNOWN, gfx::TransferFunction::Default, 0, fence, 1, 0,
      refCount, nullptr, false, gfx::HDRMetadata(), Nothing(), Nothing(),
      Nothing()));
}

// Run 3 serialize → import cycles for a single-plane RGBA surface.
static void RGBARoundtrip(RefPtr<DMABufSurface> surface) {
  SurfaceDescriptor desc;
  for (int i = 0; i < 3; i++) {
    ASSERT_TRUE(surface->Serialize(desc));
    const auto& d = desc.get_SurfaceDescriptorDMABuf();
    EXPECT_EQ(d.bufferType(), uint32_t{DMABufSurface::SURFACE_RGBA});
    EXPECT_EQ(d.fds().Length(), 1u);
    EXPECT_EQ(d.width()[0], 128u);
    EXPECT_EQ(d.height()[0], 128u);
    EXPECT_EQ(d.strides()[0], 512u);
    EXPECT_EQ(d.modifier()[0], uint64_t{0});
    surface = DMABufSurface::CreateDMABufSurface(desc);
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(), 128);
    EXPECT_EQ(surface->GetHeight(), 128);
  }
}

// Run 3 serialize → import cycles for a two-plane YUV surface.
static void YUVRoundtrip(RefPtr<DMABufSurface> surface) {
  SurfaceDescriptor desc;
  for (int i = 0; i < 3; i++) {
    ASSERT_TRUE(surface->Serialize(desc));
    const auto& d = desc.get_SurfaceDescriptorDMABuf();
    EXPECT_EQ(d.bufferType(), uint32_t{DMABufSurface::SURFACE_YUV});
    EXPECT_EQ(d.fds().Length(), 2u);
    EXPECT_EQ(d.width()[0], 128u);
    EXPECT_EQ(d.height()[0], 128u);
    EXPECT_EQ(d.width()[1], 64u);
    EXPECT_EQ(d.height()[1], 64u);
    EXPECT_EQ(d.format().Length(), 2u);
    EXPECT_EQ(d.modifier().Length(), 2u);
    surface = DMABufSurface::CreateDMABufSurface(desc);
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(0), 128);
    EXPECT_EQ(surface->GetHeight(0), 128);
    EXPECT_EQ(surface->GetWidth(1), 64);
    EXPECT_EQ(surface->GetHeight(1), 64);
  }
}

// Test both starting points for RGBA: from a descriptor (compositor side,
// verify initial import) and from a surface (GPU process side, no descriptor
// needed thanks to WGPUDMABufInfo).
TEST(DMABufSurface, RGBARoundtrip)
{
  {
    RefPtr<FileHandleWrapper> fd = MakeFd();
    ASSERT_NE(fd, nullptr);
    RefPtr<DMABufSurface> surface =
        DMABufSurface::CreateDMABufSurface(MakeRGBADescriptor(fd));
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(), 128);
    EXPECT_EQ(surface->GetHeight(), 128);
    RGBARoundtrip(surface);
  }
  {
    RefPtr<FileHandleWrapper> fd = MakeFd();
    ASSERT_NE(fd, nullptr);
    webgpu::ffi::WGPUDMABufInfo info{};
    info.is_valid = true;
    info.is_rgba = true;
    info.plane_count = 1;
    info.strides[0] = 512;
    RefPtr<DMABufSurface> surface =
        DMABufSurfaceRGBA::CreateDMABufSurface(std::move(fd), info, 128, 128);
    ASSERT_NE(surface, nullptr);
    EXPECT_EQ(surface->GetWidth(), 128);
    EXPECT_EQ(surface->GetHeight(), 128);
    RGBARoundtrip(surface);
  }
}

// YUV surfaces require VA-API hardware to create from scratch, so the surface
// is always bootstrapped via descriptor import.  Verify that import then run
// 3 serialize/import cycles from the surface side.
TEST(DMABufSurface, YUVRoundtrip)
{
  RefPtr<FileHandleWrapper> fd0 = MakeFd(), fd1 = MakeFd();
  ASSERT_NE(fd0, nullptr);
  ASSERT_NE(fd1, nullptr);
  RefPtr<DMABufSurface> surface =
      DMABufSurface::CreateDMABufSurface(MakeYUVDescriptor(fd0, fd1));
  ASSERT_NE(surface, nullptr);
  EXPECT_EQ(surface->GetWidth(0), 128);
  EXPECT_EQ(surface->GetHeight(0), 128);
  EXPECT_EQ(surface->GetWidth(1), 64);
  EXPECT_EQ(surface->GetHeight(1), 64);
  YUVRoundtrip(surface);
}

#ifdef XP_LINUX
static RefPtr<FileHandleWrapper> MakeVideoMemory(size_t aSize) {
  int fd = syscall(SYS_memfd_create, "vaapi-descriptor-test",
                   MFD_CLOEXEC | MFD_ALLOW_SEALING);
  if (fd < 0) {
    return nullptr;
  }
  auto handle = MakeRefPtr<FileHandleWrapper>(UniqueFileHandle(fd));
  if (ftruncate(fd, aSize) ||
      fcntl(fd, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL)) {
    return nullptr;
  }
  return handle;
}

static Maybe<SurfaceDescriptor> MakeVAAPIDescriptor(
    bool aSeparateObjects, uint64_t aModifier = 0,
    uint32_t aFourcc = VA_FOURCC_NV12) {
  const bool p010 = aFourcc == VA_FOURCC_P010;
  const uint32_t ySize = p010 ? 32768 : 16384;
  const uint32_t uvSize = p010 ? 16384 : 8192;
  const uint32_t stride = p010 ? 256 : 128;
  auto y = MakeVideoMemory(aSeparateObjects ? ySize : ySize + uvSize);
  auto uv = aSeparateObjects ? MakeVideoMemory(uvSize) : y;
  auto accessLock = MakeVideoMemory(sizeof(uint32_t));
  if (!y || !uv || !accessLock) {
    return Nothing();
  }
  const int uvFd = dup(uv->GetHandle());
  const int refFd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE);
  auto duplicatedUV = MakeRefPtr<FileHandleWrapper>(UniqueFileHandle(uvFd));
  auto refs = MakeRefPtr<FileHandleWrapper>(UniqueFileHandle(refFd));
  if (uvFd < 0 || refFd < 0) {
    return Nothing();
  }
  auto descriptor = MakeYUVDescriptor(y, duplicatedUV);
  auto& image = descriptor.get_SurfaceDescriptorDMABuf();
  image.fourccFormat() = aFourcc;
  image.format()[0] = p010 ? GBM_FORMAT_R16 : GBM_FORMAT_R8;
  image.format()[1] = p010 ? GBM_FORMAT_GR1616 : GBM_FORMAT_GR88;
  image.modifier()[0] = image.modifier()[1] = aModifier;
  image.strides()[0] = image.strides()[1] = stride;
  image.offsets()[1] = aSeparateObjects ? 0 : ySize;
  image.yUVColorSpace() = YUVColorSpace::BT709;
  image.colorRange() = ColorRange::FULL;
  image.chromaLocation() = 1;
  image.refCount().AppendElement(ipc::FileDescriptor(refs->GetHandle()));
  AutoTArray<DMABufVideoObject, 2> objects;
  objects.AppendElement(DMABufVideoObject(
      WrapNotNull(y), aSeparateObjects ? ySize : ySize + uvSize, aModifier));
  if (aSeparateObjects) {
    objects.AppendElement(
        DMABufVideoObject(WrapNotNull(uv), uvSize, aModifier));
  }
  AutoTArray<DMABufVideoPlane, 2> planes;
  planes.AppendElement(DMABufVideoPlane(0, 0, stride));
  planes.AppendElement(
      DMABufVideoPlane(aSeparateObjects ? 1 : 0, image.offsets()[1], stride));
  image.vaapiImageState() = Some(VAAPIImageState(
      objects, planes, 42, 7, 3, true, 226, 128, WrapNotNull(accessLock)));
  return Some(std::move(descriptor));
}

extern "C" void* wr_vulkan_register_dmabuf_device(
    const uint8_t*, const uint8_t*, const uint64_t*, size_t, const uint64_t*,
    size_t, const mozilla::wr::WrHalVideoCapabilities*);
extern "C" void wr_vulkan_unregister_dmabuf_device(void*);

TEST(DMABufSurface, RetainedVAAPIFrameSurvivesPublicationRevocation)
{
  gfxVars::Initialize();
  const bool vulkan = gfxVars::UseWebRenderVulkan();
  const bool software = gfxVars::UseSoftwareWebRender();
  const bool video = gfxVars::UseWebRenderVulkanVideo();
  const auto capabilities = gfxVars::WebRenderVulkanVideoCapabilities();
  auto restore = MakeScopeExit([&] {
    gfxVars::SetUseWebRenderVulkan(vulkan);
    gfxVars::SetUseSoftwareWebRender(software);
    gfxVars::SetUseWebRenderVulkanVideo(video);
    gfxVars::SetWebRenderVulkanVideoCapabilities(capabilities);
  });
  gfxVars::SetUseWebRenderVulkan(true);
  gfxVars::SetUseSoftwareWebRender(false);
  gfxVars::SetUseWebRenderVulkanVideo(false);
  gfxVars::SetWebRenderVulkanVideoCapabilities(VulkanVideoCapabilities());
  auto descriptor = MakeVAAPIDescriptor(false);
  ASSERT_TRUE(descriptor);
  descriptor->get_SurfaceDescriptorDMABuf().chromaLocation() = 2;
  RefPtr<DMABufSurface> surface =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(surface);
  auto* yuv = surface->GetAsDMABufSurfaceYUV();
  EXPECT_FALSE(wr::RenderCompositorVulkan::SupportsRetainedVideo(*yuv));
  wr::WrHalVideoCapabilities actual{};
  actual.drm_node[0] = 226;
  actual.drm_node[1] = 128;
  actual.format_count = 1;
  actual.formats[0] = {VA_FOURCC_NV12, 0, 128, 128, 24576};
  const uint64_t modifier = 0;
  const auto add = [&](const wr::WrHalVideoCapabilities& aCaps) {
    return wr_vulkan_register_dmabuf_device(aCaps.device_uuid,
                                            aCaps.driver_uuid, &modifier, 1,
                                            &modifier, 1, &aCaps);
  };
  void* first = add(actual);
  auto unregister =
      MakeScopeExit([&] { wr_vulkan_unregister_dmabuf_device(first); });
  EXPECT_TRUE(wr::RenderCompositorVulkan::SupportsRetainedVideo(*yuv));
  EXPECT_FALSE(wr::RenderCompositorVulkan::SupportsVideo());
  actual.drm_node[1]++;
  void* second = add(actual);
  EXPECT_FALSE(wr::RenderCompositorVulkan::SupportsRetainedVideo(*yuv));
  wr_vulkan_unregister_dmabuf_device(second);
  EXPECT_TRUE(wr::RenderCompositorVulkan::SupportsRetainedVideo(*yuv));
  surface->UnlockAccess(true);
  EXPECT_FALSE(wr::RenderCompositorVulkan::SupportsRetainedVideo(*yuv));
  EXPECT_FALSE(gfxVars::UseWebRenderVulkanVideo());
  EXPECT_TRUE(gfxVars::WebRenderVulkanVideoCapabilities().formats().IsEmpty());
}

TEST(DMABufSurface, VAAPIWaitTimeoutPreservesOwner)
{
  auto descriptor = MakeVAAPIDescriptor(false);
  ASSERT_TRUE(descriptor);
  RefPtr<DMABufSurface> surface =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(surface);
  ASSERT_TRUE(surface->WaitForAccess(0));
  EXPECT_FALSE(surface->WaitForAccess(0));
  EXPECT_FALSE(surface->WaitForAccess(10));
  EXPECT_TRUE(surface->AccessLockUsable());
  EXPECT_FALSE(surface->TryLockAccess());
  surface->UnlockAccess();
  EXPECT_TRUE(surface->WaitForAccess(0));
  surface->UnlockAccess();
}

TEST(DMABufSurface, VAAPIWaitAcquiresAfterOtherReaderCompletes)
{
  auto descriptor = MakeVAAPIDescriptor(false);
  ASSERT_TRUE(descriptor);
  RefPtr<DMABufSurface> owner = DMABufSurface::CreateDMABufSurface(*descriptor);
  RefPtr<DMABufSurface> reader =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(owner && reader);
  ASSERT_TRUE(owner->TryLockAccess());
  std::promise<void> started;
  auto waiting = std::async(std::launch::async, [&] {
    started.set_value();
    const bool acquired = reader->WaitForAccess(5000);
    if (acquired) reader->UnlockAccess();
    return acquired;
  });
  started.get_future().wait();
  EXPECT_EQ(waiting.wait_for(std::chrono::milliseconds(20)),
            std::future_status::timeout);
  owner->UnlockAccess();
  EXPECT_TRUE(waiting.get());
  EXPECT_TRUE(owner->TryLockAccess());
  owner->UnlockAccess();
}

TEST(DMABufSurface, VAAPIWaitStopsOnAbandonment)
{
  auto descriptor = MakeVAAPIDescriptor(false);
  ASSERT_TRUE(descriptor);
  RefPtr<DMABufSurface> owner = DMABufSurface::CreateDMABufSurface(*descriptor);
  RefPtr<DMABufSurface> reader =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(owner && reader);
  ASSERT_TRUE(owner->TryLockAccess());
  std::promise<void> started;
  auto waiting = std::async(std::launch::async, [&] {
    started.set_value();
    return reader->WaitForAccess(5000);
  });
  started.get_future().wait();
  EXPECT_EQ(waiting.wait_for(std::chrono::milliseconds(20)),
            std::future_status::timeout);
  owner->UnlockAccess(true);
  EXPECT_FALSE(waiting.get());
  EXPECT_FALSE(reader->AccessLockUsable());
}

TEST(DMABufSurface, DISABLED_NativeVAAPIGLReaders)
{
  const char* format = getenv("WR_VIDEO_FORMAT");
  const bool p010 = format && !strcmp(format, "P010");
  if (format) {
    ASSERT_TRUE(p010 || !strcmp(format, "NV12"));
  }
  const char* prefix = p010 ? "WR_P010" : "WR_NV12";
  const auto name = [&](const char* suffix) {
    return std::string(prefix) + "_" + suffix;
  };
  ASSERT_NE(getenv(name("FD").c_str()), nullptr) << "Requires ExportVAAPIFrame";
  const auto number = [&](const char* suffix) -> uint64_t {
    const auto key = name(suffix);
    const char* value = getenv(key.c_str());
    EXPECT_NE(value, nullptr) << key;
    return value ? strtoull(value, nullptr, 10) : 0;
  };
  gfxVars::Initialize();
  const bool software = gfxVars::UseSoftwareWebRender();
  const bool egl = gfxVars::UseEGL();
  gfxVars::SetUseSoftwareWebRender(false);
  gfxVars::SetUseEGL(true);
  auto restore = MakeScopeExit([&] {
    DMABufSurface::ReleaseSnapshotGLContext();
    gfxVars::SetUseSoftwareWebRender(software);
    gfxVars::SetUseEGL(egl);
  });
  auto descriptor =
      MakeVAAPIDescriptor(false, 0, p010 ? VA_FOURCC_P010 : VA_FOURCC_NV12);
  ASSERT_TRUE(descriptor);
  auto fd = MakeRefPtr<FileHandleWrapper>(UniqueFileHandle(dup(number("FD"))));
  ASSERT_GE(fd->GetHandle(), 0);
  auto& image = descriptor->get_SurfaceDescriptorDMABuf();
  auto& state = image.vaapiImageState().ref();
  state.drmRenderMajor() = number("DRM_MAJOR");
  state.drmRenderMinor() = number("DRM_MINOR");
  state.objects()[0].fd() = WrapNotNull(fd);
  state.objects()[0].size() = number("BYTES");
  state.objects()[0].modifier() = number("MODIFIER");
  image.colorRange() = ColorRange::LIMITED;
  for (size_t i = 0; i < 2; ++i) {
    image.fds()[i] = WrapNotNull(fd);
    image.width()[i] = number("WIDTH") >> i;
    image.height()[i] = number("HEIGHT") >> i;
    image.widthAligned()[i] = number("ALLOC_WIDTH") >> i;
    image.heightAligned()[i] = number("ALLOC_HEIGHT") >> i;
    image.modifier()[i] = state.objects()[0].modifier();
    image.strides()[i] = number(i ? "UV_PITCH" : "Y_PITCH");
    image.offsets()[i] = number(i ? "UV_OFFSET" : "Y_OFFSET");
    state.planes()[i].stride() = image.strides()[i];
    state.planes()[i].offset() = image.offsets()[i];
  }
  const auto referenceName = name("REFERENCE");
  const char* referencePath = getenv(referenceName.c_str());
  ASSERT_NE(referencePath, nullptr);
  FILE* reference = fopen(referencePath, "rb");
  ASSERT_NE(reference, nullptr);
  auto closeReference = MakeScopeExit([&] { fclose(reference); });
  const int width = image.width()[0];
  const int height = image.height()[0];
  nsTArray<uint8_t> pixels;
  const size_t imagePixels = size_t(width) * height;
  pixels.SetLength(p010 ? imagePixels * 3 : imagePixels * 3 / 2);
  ASSERT_EQ(fread(pixels.Elements(), 1, pixels.Length(), reference),
            pixels.Length());
  const auto sample = [&](size_t offset) -> double {
    if (!p010) {
      return pixels[offset];
    }
    const uint16_t word =
        uint16_t(pixels[offset]) | (uint16_t(pixels[offset + 1]) << 8);
    EXPECT_EQ(word & uint16_t{0x3f}, uint16_t{0});
    return word >> 6;
  };
  const auto luma = [&](int x, int y) {
    return sample((size_t(y) * width + x) * (p010 ? 2 : 1));
  };
  const auto chroma = [&](int x, int y, int component) {
    const size_t yBytes = size_t(width) * height * (p010 ? 2 : 1);
    const size_t offset =
        yBytes + ((size_t(y / 2) * (width / 2) + x / 2) * 2 + component) *
                     (p010 ? 2 : 1);
    return sample(offset);
  };
  RefPtr<DMABufSurface> native;
  for (auto space : {YUVColorSpace::BT601, YUVColorSpace::BT709}) {
    for (auto range : {ColorRange::LIMITED, ColorRange::FULL}) {
      SCOPED_TRACE(int(space));
      SCOPED_TRACE(int(range));
      image.yUVColorSpace() = space;
      image.colorRange() = range;
      native = DMABufSurface::CreateDMABufSurface(*descriptor);
      ASSERT_TRUE(native);
      auto legacyDescriptor = *descriptor;
      legacyDescriptor.get_SurfaceDescriptorDMABuf().vaapiImageState().reset();
      RefPtr<DMABufSurface> legacy =
          DMABufSurface::CreateDMABufSurface(legacyDescriptor);
      ASSERT_TRUE(legacy);
      RefPtr<gfx::DataSourceSurface> expected = legacy->GetAsSourceSurface();
      RefPtr<gfx::DataSourceSurface> actual = native->GetAsSourceSurface();
      ASSERT_TRUE(expected);
      ASSERT_TRUE(actual);
      gfx::DataSourceSurface::ScopedMap expectedMap(
          expected, gfx::DataSourceSurface::READ);
      gfx::DataSourceSurface::ScopedMap actualMap(actual,
                                                  gfx::DataSourceSurface::READ);
      ASSERT_TRUE(expectedMap.IsMapped());
      ASSERT_TRUE(actualMap.IsMapped());
      const double kr = space == YUVColorSpace::BT601 ? 0.299 : 0.2126;
      const double kb = space == YUVColorSpace::BT601 ? 0.114 : 0.0722;
      const double kg = 1 - kr - kb;
      const bool full = range == ColorRange::FULL;
      int maxError = 0;
      for (int y = 0; y < height; ++y) {
        const auto* row = actualMap.GetData() + y * actualMap.GetStride();
        EXPECT_EQ(memcmp(expectedMap.GetData() + y * expectedMap.GetStride(),
                         row, width * 4),
                  0)
            << y;
        for (int x = 0; x < width; ++x) {
          const double scale = p010 ? 4 : 1;
          const double maximum = p010 ? 1023 : 255;
          const double center = p010 ? 512 : 128;
          const double yy = full ? luma(x, y) / maximum
                                 : (luma(x, y) - 16 * scale) / (219 * scale);
          const double cb =
              (chroma(x, y, 0) - center) / (full ? maximum : 224 * scale);
          const double cr =
              (chroma(x, y, 1) - center) / (full ? maximum : 224 * scale);
          const double bgr[] = {
              (yy + 2 * (1 - kb) * cb) * 255,
              (yy - 2 * kb * (1 - kb) / kg * cb - 2 * kr * (1 - kr) / kg * cr) *
                  255,
              (yy + 2 * (1 - kr) * cr) * 255};
          for (size_t c = 0; c < 3; ++c) {
            const int value = std::lround(std::clamp(bgr[c], 0.0, 255.0));
            maxError =
                std::max(maxError, std::abs(int(row[x * 4 + c]) - value));
          }
          maxError = std::max(maxError, std::abs(int(row[x * 4 + 3]) - 255));
        }
      }
      EXPECT_LE(maxError, 2);
    }
  }
  ASSERT_TRUE(native->TryLockAccess());
  std::promise<void> readerStarted;
  auto concurrentRead = std::async(std::launch::async, [&] {
    readerStarted.set_value();
    RefPtr<gfx::DataSourceSurface> result = native->GetAsSourceSurface();
    return result;
  });
  readerStarted.get_future().wait();
  EXPECT_EQ(concurrentRead.wait_for(std::chrono::milliseconds(20)),
            std::future_status::timeout);
  native->UnlockAccess();
  EXPECT_TRUE(concurrentRead.get());
  ASSERT_TRUE(native->TryLockAccess());
  auto releaseAccess = MakeScopeExit([&] { native->UnlockAccess(); });
  RefPtr<gfx::DataSourceSurface> snapshot = native->GetAsSourceSurface();
  EXPECT_FALSE(snapshot);
  EXPECT_TRUE(native->AccessLockUsable());
  EXPECT_FALSE(native->TryLockAccess());
  nsCString failure;
  RefPtr<gl::GLContext> context =
      gl::GLContextProviderEGL::CreateHeadless({}, &failure);
  ASSERT_TRUE(context)
  << failure.get();
  ASSERT_TRUE(context->MakeCurrent());
  EXPECT_FALSE(context->BlitHelper()->BlitSdToFramebuffer(
      *descriptor, gfx::IntRect(0, 0, native->GetWidth(), native->GetHeight()),
      gl::OriginPos::BottomLeft));
  EXPECT_TRUE(native->AccessLockUsable());
  native->UnlockAccess();
  releaseAccess.release();
  snapshot = native->GetAsSourceSurface();
  EXPECT_TRUE(snapshot);
  native->UnlockAccess(true);
  snapshot = native->GetAsSourceSurface();
  EXPECT_FALSE(snapshot);
}

TEST(DMABufSurface, VAAPICapabilitiesRejectUnsupportedFrames)
{
  VulkanVideoCapabilities capabilities;
  capabilities.drmMajor() = 226;
  capabilities.drmMinor() = 128;
  capabilities.deviceUUID().SetLength(16);
  capabilities.driverUUID().SetLength(16);
  capabilities.formats().AppendElement(
      VulkanVideoFormat(VA_FOURCC_NV12, 0, 128, 128, 24576));
  const auto supports = [&](const SurfaceDescriptor& aDescriptor,
                            const VulkanVideoCapabilities& aCapabilities) {
    RefPtr<DMABufSurface> surface =
        DMABufSurface::CreateDMABufSurface(aDescriptor);
    EXPECT_TRUE(surface);
    return surface &&
           surface->GetAsDMABufSurfaceYUV()->SupportsVAAPIImage(aCapabilities);
  };
  auto descriptor = MakeVAAPIDescriptor(false, 0);
  ASSERT_TRUE(descriptor);
  descriptor->get_SurfaceDescriptorDMABuf().chromaLocation() = 2;
  EXPECT_TRUE(supports(*descriptor, capabilities));
  for (int i = 0; i < 8; ++i) {
    SCOPED_TRACE(i);
    auto changed = capabilities;
    switch (i) {
      case 0:
        changed.drmMinor()++;
        break;
      case 1:
        changed.deviceUUID().Clear();
        break;
      case 2:
        changed.driverUUID().Clear();
        break;
      case 3:
        changed.formats().Clear();
        break;
      case 4:
        changed.formats()[0].modifier() = 1;
        break;
      case 5:
        changed.formats()[0].maxWidth()--;
        break;
      case 6:
        changed.formats()[0].maxHeight()--;
        break;
      case 7:
        changed.formats()[0].maxAllocationSize()--;
        break;
    }
    EXPECT_FALSE(supports(*descriptor, changed));
  }
  for (auto transfer : {TransferFunction::PQ, TransferFunction::HLG}) {
    auto changed = *descriptor;
    changed.get_SurfaceDescriptorDMABuf().transferFunction() = transfer;
    EXPECT_FALSE(supports(changed, capabilities));
  }
  for (auto primaries : {ColorSpace2::DISPLAY_P3, ColorSpace2::BT2020}) {
    auto changed = *descriptor;
    changed.get_SurfaceDescriptorDMABuf().colorPrimaries() = primaries;
    EXPECT_FALSE(supports(changed, capabilities));
  }
  auto separate = MakeVAAPIDescriptor(true, 0);
  ASSERT_TRUE(separate);
  EXPECT_FALSE(supports(*separate, capabilities));

  auto unspecified = *descriptor;
  unspecified.get_SurfaceDescriptorDMABuf().chromaLocation() = 0;
  EXPECT_TRUE(supports(unspecified, capabilities));
  auto left = *descriptor;
  left.get_SurfaceDescriptorDMABuf().chromaLocation() = 1;
  EXPECT_FALSE(supports(left, capabilities));

  auto p010 = MakeVAAPIDescriptor(false, 0, VA_FOURCC_P010);
  ASSERT_TRUE(p010);
  p010->get_SurfaceDescriptorDMABuf().chromaLocation() = 2;
  auto p010Capabilities = capabilities;
  p010Capabilities.formats()[0] =
      VulkanVideoFormat(VA_FOURCC_P010, 0, 128, 128, 49152);
  EXPECT_TRUE(supports(*p010, p010Capabilities));
  EXPECT_FALSE(supports(*p010, capabilities));
  EXPECT_FALSE(supports(*descriptor, p010Capabilities));
  for (auto* candidate : {&*descriptor, &*p010}) {
    auto hdr = *candidate;
    hdr.get_SurfaceDescriptorDMABuf().hdrMetadata().mContentLightLevel =
        Some(ContentLightLevel{1000, 400});
    EXPECT_FALSE(supports(
        hdr, candidate == &*descriptor ? capabilities : p010Capabilities));
  }
}

TEST(DMABufSurface, VAAPIObjectAndPlaneRoundtrip)
{
  for (bool separateObjects : {false, true}) {
    for (uint64_t modifier : {uint64_t{0}, uint64_t{0x0100000000000002}}) {
      auto descriptor = MakeVAAPIDescriptor(separateObjects, modifier);
      ASSERT_TRUE(descriptor);
      for (int i = 0; i < 3; ++i) {
        RefPtr<DMABufSurface> surface =
            DMABufSurface::CreateDMABufSurface(*descriptor);
        ASSERT_TRUE(surface);
        ASSERT_TRUE(surface->GetAsDMABufSurfaceYUV()->GetVAAPIDescriptor());
        ASSERT_TRUE(surface->Serialize(*descriptor));
        const auto& image = descriptor->get_SurfaceDescriptorDMABuf();
        ASSERT_TRUE(image.vaapiImageState());
        const auto& state = image.vaapiImageState().ref();
        ASSERT_EQ(state.objects().Length(), separateObjects ? 2u : 1u);
        ASSERT_EQ(state.planes().Length(), 2u);
        EXPECT_EQ(state.objects()[0].size(), separateObjects ? 16384u : 24576u);
        EXPECT_EQ(state.objects()[0].modifier(), modifier);
        EXPECT_EQ(state.planes()[1].objectIndex(), separateObjects ? 1u : 0u);
        EXPECT_EQ(state.planes()[1].offset(), separateObjects ? 0u : 16384u);
        EXPECT_EQ(state.planes()[1].stride(), 128u);
        EXPECT_EQ(state.allocationId(), 42u);
        EXPECT_EQ(state.generation(), 7u);
        EXPECT_EQ(state.producerEpoch(), 3u);
        EXPECT_EQ(state.drmRenderMajor(), 226u);
        EXPECT_EQ(state.drmRenderMinor(), 128u);
        EXPECT_TRUE(state.producerComplete());
        EXPECT_EQ(image.yUVColorSpace(), YUVColorSpace::BT709);
        EXPECT_EQ(image.colorRange(), ColorRange::FULL);
        EXPECT_EQ(image.chromaLocation(), 1u);
      }
    }
  }
}

TEST(DMABufSurface, P010ObjectPlaneAndMetadataRoundtrip)
{
  auto descriptor = MakeVAAPIDescriptor(false, 0, VA_FOURCC_P010);
  ASSERT_TRUE(descriptor);
  auto& original = descriptor->get_SurfaceDescriptorDMABuf();
  original.colorPrimaries() = ColorSpace2::BT709;
  original.transferFunction() = TransferFunction::BT709;
  original.chromaLocation() = 1;
  original.hdrMetadata().mContentLightLevel =
      Some(ContentLightLevel{1000, 400});
  for (int i = 0; i < 3; ++i) {
    RefPtr<DMABufSurface> surface =
        DMABufSurface::CreateDMABufSurface(*descriptor);
    ASSERT_TRUE(surface);
    ASSERT_TRUE(surface->Serialize(*descriptor));
    const auto& image = descriptor->get_SurfaceDescriptorDMABuf();
    ASSERT_TRUE(image.vaapiImageState());
    EXPECT_EQ(image.fourccFormat(), uint32_t(VA_FOURCC_P010));
    EXPECT_EQ(image.format()[0], GBM_FORMAT_R16);
    EXPECT_EQ(image.format()[1], GBM_FORMAT_GR1616);
    EXPECT_EQ(image.strides()[0], 256u);
    EXPECT_EQ(image.strides()[1], 256u);
    EXPECT_EQ(image.offsets()[1], 32768u);
    EXPECT_EQ(image.vaapiImageState()->objects()[0].size(), 49152u);
    EXPECT_EQ(image.colorPrimaries(), ColorSpace2::BT709);
    EXPECT_EQ(image.transferFunction(), TransferFunction::BT709);
    EXPECT_EQ(image.chromaLocation(), 1u);
    EXPECT_EQ(image.hdrMetadata().mContentLightLevel,
              Some(ContentLightLevel{1000, 400}));
  }
}

TEST(DMABufSurface, P010RejectsInvalidBoundsAndWordAlignment)
{
  using Mutate = void (*)(SurfaceDescriptorDMABuf&);
  const Mutate mutations[] = {
      [](auto& d) {
        d.strides()[0] = 255;
        d.vaapiImageState()->planes()[0].stride() = 255;
      },
      [](auto& d) {
        d.offsets()[1] = 32769;
        d.vaapiImageState()->planes()[1].offset() = 32769;
      },
      [](auto& d) { d.vaapiImageState()->objects()[0].size() = 49151; },
      [](auto& d) { d.width()[0] = 127; },
      [](auto& d) { d.widthAligned()[0] = 126; },
      [](auto& d) { d.format()[1] = GBM_FORMAT_GR88; },
  };
  for (const auto& mutate : mutations) {
    auto descriptor = MakeVAAPIDescriptor(false, 0, VA_FOURCC_P010);
    ASSERT_TRUE(descriptor);
    mutate(descriptor->get_SurfaceDescriptorDMABuf());
    RefPtr<DMABufSurface> rejected =
        DMABufSurface::CreateDMABufSurface(*descriptor);
    EXPECT_FALSE(rejected);
  }
}

TEST(DMABufSurface, VAAPIRejectsInvalidDescriptors)
{
  using Mutate = void (*)(SurfaceDescriptorDMABuf&);
  const struct {
    const char* name;
    Mutate mutate;
  } cases[] = {
      {"wrong surface type",
       [](auto& d) { d.bufferType() = DMABufSurface::SURFACE_RGBA; }},
      {"wrong fourcc", [](auto& d) { d.fourccFormat() = GBM_FORMAT_ARGB8888; }},
      {"wrong plane format", [](auto& d) { d.format()[1] = GBM_FORMAT_R8; }},
      {"missing plane",
       [](auto& d) { d.vaapiImageState()->planes().RemoveLastElement(); }},
      {"missing object",
       [](auto& d) { d.vaapiImageState()->objects().Clear(); }},
      {"bad object index",
       [](auto& d) { d.vaapiImageState()->planes()[1].objectIndex() = 1; }},
      {"duplicate object",
       [](auto& d) {
         auto object = d.vaapiImageState()->objects()[0];
         d.vaapiImageState()->objects().AppendElement(object);
       }},
      {"wrong allocation size",
       [](auto& d) { d.vaapiImageState()->objects()[0].size()++; }},
      {"unsupported modifier",
       [](auto& d) {
         d.vaapiImageState()->objects()[0].modifier() = UINT64_MAX;
         d.modifier()[0] = d.modifier()[1] = UINT64_MAX;
       }},
      {"wrong legacy modifier", [](auto& d) { d.modifier()[1] = 1; }},
      {"wrong legacy stride", [](auto& d) { d.strides()[1]++; }},
      {"wrong legacy offset", [](auto& d) { d.offsets()[1]++; }},
      {"missing dimensions", [](auto& d) { d.heightAligned().Clear(); }},
      {"too many planes",
       [](auto& d) {
         auto fd = d.fds()[0];
         for (int i = 0; i < 4; ++i) d.fds().AppendElement(fd);
       }},
      {"zero width", [](auto& d) { d.width()[0] = 0; }},
      {"odd width", [](auto& d) { d.width()[0] = 127; }},
      {"wrong chroma dimensions", [](auto& d) { d.width()[1]++; }},
      {"oversized allocation dimensions",
       [](auto& d) { d.widthAligned()[0] = UINT32_MAX; }},
      {"crop outside allocation", [](auto& d) { d.width()[0] = 130; }},
      {"short pitch",
       [](auto& d) {
         d.strides()[0] = 64;
         d.vaapiImageState()->planes()[0].stride() = 64;
       }},
      {"offset outside allocation",
       [](auto& d) {
         d.offsets()[1] = 24576;
         d.vaapiImageState()->planes()[1].offset() = 24576;
       }},
      {"linear extent outside allocation",
       [](auto& d) {
         d.strides()[1] = 256;
         d.vaapiImageState()->planes()[1].stride() = 256;
       }},
      {"overlapping planes",
       [](auto& d) {
         d.offsets()[1] = 8192;
         d.vaapiImageState()->planes()[1].offset() = 8192;
       }},
      {"overflowing offset",
       [](auto& d) { d.vaapiImageState()->planes()[1].offset() = UINT64_MAX; }},
      {"zero allocation identity",
       [](auto& d) { d.vaapiImageState()->allocationId() = 0; }},
      {"zero generation",
       [](auto& d) { d.vaapiImageState()->generation() = 0; }},
      {"zero producer epoch",
       [](auto& d) { d.vaapiImageState()->producerEpoch() = 0; }},
      {"missing DRM device",
       [](auto& d) { d.vaapiImageState()->drmRenderMajor() = 0; }},
      {"invalid DRM minor",
       [](auto& d) { d.vaapiImageState()->drmRenderMinor() = UINT64_MAX; }},
      {"producer not complete",
       [](auto& d) { d.vaapiImageState()->producerComplete() = false; }},
      {"unexpected fence",
       [](auto& d) { d.fence().AppendElement(d.fds()[0]); }},
      {"unexpected semaphore", [](auto& d) { d.semaphoreFd() = d.fds()[0]; }},
      {"unexpected sync-file tag",
       [](auto& d) { d.semaphoreFdIsSyncFd() = true; }},
      {"missing lifetime reference", [](auto& d) { d.refCount().Clear(); }},
      {"invalid access lock",
       [](auto& d) { d.vaapiImageState()->accessLock() = d.fds()[0]; }},
      {"conflicting producer",
       [](auto& d) {
         d.foreignRGBImageState() =
             Some(ForeignRGBImageState(1, d.vaapiImageState()->accessLock()));
       }},
  };
  for (const auto& test : cases) {
    SCOPED_TRACE(test.name);
    auto descriptor = MakeVAAPIDescriptor(false);
    ASSERT_TRUE(descriptor);
    test.mutate(descriptor->get_SurfaceDescriptorDMABuf());
    RefPtr<DMABufSurface> surface =
        DMABufSurface::CreateDMABufSurface(*descriptor);
    EXPECT_FALSE(surface);
  }
}

TEST(DMABufSurface, VAAPIRejectsMismatchedObjectHandle)
{
  auto descriptor = MakeVAAPIDescriptor(true);
  ASSERT_TRUE(descriptor);
  auto& image = descriptor->get_SurfaceDescriptorDMABuf();
  image.fds()[1] = image.fds()[0];
  RefPtr<DMABufSurface> surface =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  EXPECT_FALSE(surface);
}

TEST(DMABufSurface, VAAPIAcceptsLargerBackingAllocation)
{
  auto descriptor = MakeVAAPIDescriptor(false);
  auto larger = MakeVideoMemory(32768);
  ASSERT_TRUE(descriptor);
  ASSERT_TRUE(larger);
  auto& image = descriptor->get_SurfaceDescriptorDMABuf();
  image.fds()[0] = image.fds()[1] = WrapNotNull(larger);
  image.vaapiImageState()->objects()[0].fd() = WrapNotNull(larger);
  RefPtr<DMABufSurface> surface =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(surface);
  SurfaceDescriptor forwarded;
  ASSERT_TRUE(surface->Serialize(forwarded));
  EXPECT_EQ(forwarded.get_SurfaceDescriptorDMABuf()
                .vaapiImageState()
                ->objects()[0]
                .size(),
            24576u);
}

TEST(DMABufSurface, VAAPIAbandonmentPreventsForwarding)
{
  auto descriptor = MakeVAAPIDescriptor(false);
  ASSERT_TRUE(descriptor);
  RefPtr<DMABufSurface> first = DMABufSurface::CreateDMABufSurface(*descriptor);
  RefPtr<DMABufSurface> second =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(first);
  ASSERT_TRUE(second);
  ASSERT_TRUE(first->LockAccess());
  first->UnlockAccess(true);
  EXPECT_FALSE(second->AccessLockUsable());
  SurfaceDescriptor forwarded;
  EXPECT_FALSE(second->Serialize(forwarded));
  RefPtr<DMABufSurface> rejected =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  EXPECT_FALSE(rejected);
}

TEST(DMABufSurface, VAAPITryLockDoesNotPoisonBusyPublication)
{
  auto descriptor = MakeVAAPIDescriptor(false);
  ASSERT_TRUE(descriptor);
  RefPtr<DMABufSurface> first = DMABufSurface::CreateDMABufSurface(*descriptor);
  RefPtr<DMABufSurface> second =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(first);
  ASSERT_TRUE(second);
  ASSERT_TRUE(first->TryLockAccess());
  EXPECT_FALSE(second->TryLockAccess());
  EXPECT_TRUE(first->AccessLockUsable());
  first->UnlockAccess();
  ASSERT_TRUE(second->TryLockAccess());
  second->UnlockAccess(true);
  EXPECT_FALSE(first->TryLockAccess());
  EXPECT_FALSE(first->AccessLockUsable());
}

TEST(DMABufSurface, VAAPIExportsOneFrameForBothHalChannels)
{
  for (bool separate : {false, true}) {
    auto descriptor = MakeVAAPIDescriptor(separate);
    ASSERT_TRUE(descriptor);
    RefPtr<DMABufSurface> surface =
        DMABufSurface::CreateDMABufSurface(*descriptor);
    ASSERT_TRUE(surface);
    wr::WrHalImage image{};
    const auto getImage = [&](uint8_t aChannel) {
      return wr::RenderDMABUFTextureHost::GetVAAPIImage(
          *surface->GetAsDMABufSurfaceYUV(), aChannel, &image);
    };
    for (uint8_t channel : {1, 0, 1}) {
      ASSERT_EQ(getImage(channel), !separate);
      if (separate) continue;
      ASSERT_TRUE(image.source.IsVideo());
      const auto& video = image.source.video._0;
      EXPECT_EQ(image.generation, 7u);
      EXPECT_EQ(video.fourcc, uint32_t(VA_FOURCC_NV12));
      EXPECT_EQ(video.width, 128u);
      EXPECT_EQ(video.height, 128u);
      EXPECT_EQ(video.allocation_width, 128u);
      EXPECT_EQ(video.allocation_height, 128u);
      EXPECT_EQ(video.allocation_size, 24576u);
      EXPECT_EQ(video.offsets[1], 16384u);
      EXPECT_EQ(video.strides[1], 128u);
      EXPECT_EQ(video.allocation_id, 42u);
      EXPECT_EQ(video.producer_epoch, 3u);
      EXPECT_EQ(video.drm_node[0], 226u);
      EXPECT_EQ(video.drm_node[1], 128u);
      EXPECT_GE(video.fd, 0);
      EXPECT_GE(video.access_lock_fd, 0);
      ASSERT_TRUE(surface->TryLockAccess());
      surface->UnlockAccess();
    }
    EXPECT_FALSE(getImage(2));
    ASSERT_TRUE(surface->TryLockAccess());
    surface->UnlockAccess(true);
    EXPECT_FALSE(getImage(0));
  }
}

TEST(DMABufSurface, P010ExportsOneFrameForBothHalChannels)
{
  auto descriptor = MakeVAAPIDescriptor(false, 0, VA_FOURCC_P010);
  ASSERT_TRUE(descriptor);
  RefPtr<DMABufSurface> surface =
      DMABufSurface::CreateDMABufSurface(*descriptor);
  ASSERT_TRUE(surface);
  for (uint8_t channel : {1, 0, 1}) {
    wr::WrHalImage image{};
    ASSERT_TRUE(wr::RenderDMABUFTextureHost::GetVAAPIImage(
        *surface->GetAsDMABufSurfaceYUV(), channel, &image));
    ASSERT_TRUE(image.source.IsVideo());
    const auto& video = image.source.video._0;
    EXPECT_EQ(video.fourcc, uint32_t(VA_FOURCC_P010));
    EXPECT_EQ(video.allocation_size, 49152u);
    EXPECT_EQ(video.strides[0], 256u);
    EXPECT_EQ(video.strides[1], 256u);
    EXPECT_EQ(video.offsets[1], 32768u);
  }
}

TEST(DMABufSurface, VAAPIPublicationIdentitySurvivesForwarding)
{
  auto original = MakeVAAPIDescriptor(false);
  ASSERT_TRUE(original);
  auto next = *original;
  auto& state = next.get_SurfaceDescriptorDMABuf().vaapiImageState().ref();
  state.generation()++;
  state.producerEpoch()++;
  RefPtr<DMABufSurface> surface = DMABufSurface::CreateDMABufSurface(next);
  ASSERT_TRUE(surface);
  SurfaceDescriptor forwarded;
  ASSERT_TRUE(surface->Serialize(forwarded));
  const auto& result =
      forwarded.get_SurfaceDescriptorDMABuf().vaapiImageState().ref();
  EXPECT_EQ(result.allocationId(), 42u);
  EXPECT_EQ(result.generation(), 8u);
  EXPECT_EQ(result.producerEpoch(), 4u);
  EXPECT_EQ(
      original->get_SurfaceDescriptorDMABuf().vaapiImageState()->generation(),
      7u);
}

TEST(DMABufSurface, VAAPIExportCleanupClosesUnreferencedObjects)
{
  int firstPipe[2], secondPipe[2];
  ASSERT_EQ(pipe2(firstPipe, O_CLOEXEC | O_NONBLOCK), 0);
  UniqueFileHandle firstRead(firstPipe[0]), firstWrite(firstPipe[1]);
  ASSERT_EQ(pipe2(secondPipe, O_CLOEXEC | O_NONBLOCK), 0);
  UniqueFileHandle secondRead(secondPipe[0]), secondWrite(secondPipe[1]);
  VADRMPRIMESurfaceDescriptor descriptor{};
  descriptor.num_objects = 2;
  descriptor.objects[0].fd = firstWrite.release();
  descriptor.objects[1].fd = secondWrite.release();
  descriptor.num_layers = 1;
  descriptor.layers[0].object_index[0] = 0;
  DMABufSurfaceYUV::ReleaseVADRMPRIMESurfaceDescriptor(descriptor);
  EXPECT_EQ(descriptor.objects[0].fd, -1);
  EXPECT_EQ(descriptor.objects[1].fd, -1);
  char byte;
  EXPECT_EQ(read(firstRead.get(), &byte, 1), 0);
  EXPECT_EQ(read(secondRead.get(), &byte, 1), 0);
}

static void AddVulkanState(SurfaceDescriptor& aDescriptor,
                           RefPtr<FileHandleWrapper> aAccessLock) {
  AutoTArray<uint8_t, 16> identity = {0, 0, 0, 0, 0, 0, 0, 0,
                                      0, 0, 0, 0, 0, 0, 0, 0};
  auto& image = aDescriptor.get_SurfaceDescriptorDMABuf();
  image.semaphoreFdIsSyncFd() = true;
  image.vulkanImageState() =
      Some(VulkanImageState(identity, identity, 1, WrapNotNull(aAccessLock)));
}

TEST(DMABufSurface, VulkanRejectsInvalidAccessLock)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  AddVulkanState(descriptor, fd);
  RefPtr<DMABufSurface> rejected =
      DMABufSurface::CreateDMABufSurface(descriptor);
  EXPECT_FALSE(rejected);
}

TEST(DMABufSurface, VulkanSnapshotSerializesCompositorAccess)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  RefPtr<DMABufSurface> snapshot =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(snapshot);
  ASSERT_TRUE(snapshot->CreateAccessLock());
  AddVulkanState(descriptor, snapshot->GetAccessLockFd());
  RefPtr<DMABufSurface> imported =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(imported);
  SurfaceDescriptor forwarded;
  ASSERT_TRUE(imported->Serialize(forwarded));
  RefPtr<DMABufSurface> compositor =
      DMABufSurface::CreateDMABufSurface(forwarded);
  ASSERT_TRUE(compositor);

  ASSERT_TRUE(snapshot->LockAccess());
  std::promise<void> started;
  auto acquired = std::async(std::launch::async, [&] {
    started.set_value();
    const bool locked = compositor->LockAccess();
    if (locked) {
      compositor->UnlockAccess();
    }
    return locked;
  });
  started.get_future().wait();
  EXPECT_EQ(acquired.wait_for(std::chrono::milliseconds(100)),
            std::future_status::timeout);
  snapshot->UnlockAccess();
  EXPECT_TRUE(acquired.get());
  ASSERT_TRUE(snapshot->LockAccess());
  snapshot->UnlockAccess();
}

TEST(DMABufSurface, VulkanAbandonmentPreventsProducerRecycling)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  RefPtr<DMABufSurface> producer =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(producer);
  ASSERT_TRUE(producer->CreateAccessLock());
  AddVulkanState(descriptor, producer->GetAccessLockFd());
  RefPtr<DMABufSurface> compositor =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(compositor);

  webgpu::ffi::WGPUTextureFormat format{};
  format.tag = webgpu::ffi::WGPUTextureFormat_Bgra8Unorm;
  webgpu::ffi::WGPUDMABufInfo info{};
  info.is_valid = true;
  info.for_webrender = true;
  webgpu::SharedTextureDMABuf texture(
      128, 128, format, {}, RefPtr<DMABufSurface>(producer),
      descriptor.get_SurfaceDescriptorDMABuf(), info);
  EXPECT_TRUE(texture.GetDMABufInfo().is_valid);

  ASSERT_TRUE(compositor->LockAccess());
  compositor->UnlockAccess();
  EXPECT_TRUE(producer->AccessLockUsable());
  ASSERT_TRUE(compositor->LockAccess());
  compositor->UnlockAccess(true);
  EXPECT_FALSE(producer->AccessLockUsable());
  EXPECT_FALSE(producer->LockAccess());
  EXPECT_FALSE(compositor->LockAccess());
  EXPECT_FALSE(texture.GetDMABufInfo().is_valid);
  EXPECT_FALSE(texture.CloneDmaBufFd());
  EXPECT_FALSE(texture.RetireVulkanPublication());
  EXPECT_FALSE(texture.CanRetryVulkanRetirement());
  texture.CleanForRecycling();
  EXPECT_FALSE(texture.GetDMABufInfo().is_valid);
  EXPECT_FALSE(producer->CreateAccessLock());
  producer->UnlockAccess();
  EXPECT_FALSE(producer->AccessLockUsable());
}

TEST(DMABufSurface, VulkanRecyclingRetiresOldReaders)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  RefPtr<DMABufSurface> producer =
      DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(producer);
  ASSERT_TRUE(producer->CreateAccessLock());
  AddVulkanState(descriptor, producer->GetAccessLockFd());
  RefPtr<DMABufSurface> reader = DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(reader);
  webgpu::ffi::WGPUTextureFormat format{};
  format.tag = webgpu::ffi::WGPUTextureFormat_Bgra8Unorm;
  webgpu::ffi::WGPUDMABufInfo info{};
  info.is_valid = true;
  info.for_webrender = true;
  webgpu::SharedTextureDMABuf texture(
      128, 128, format, {}, RefPtr<DMABufSurface>(producer),
      descriptor.get_SurfaceDescriptorDMABuf(), info);

  ASSERT_TRUE(reader->TryLockAccess());
  EXPECT_FALSE(texture.RetireVulkanPublication());
  EXPECT_TRUE(texture.CanRetryVulkanRetirement());
  EXPECT_TRUE(producer->AccessLockUsable());
  reader->UnlockAccess();
  ASSERT_TRUE(texture.RetireVulkanPublication());
  EXPECT_FALSE(producer->AccessLockUsable());
  EXPECT_FALSE(reader->TryLockAccess());
  EXPECT_FALSE(reader->WaitForAccess(1));
  EXPECT_TRUE(texture.GetDMABufInfo().is_valid);
  EXPECT_TRUE(texture.CanRetryVulkanRetirement());
  EXPECT_TRUE(texture.CloneDmaBufFd());
  texture.CleanForRecycling();
  EXPECT_TRUE(texture.RetireVulkanPublication());
  EXPECT_FALSE(reader->AccessLockUsable());
}

TEST(DMABufSurface, ForeignRGBRejectsMissingAccessLock)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  auto descriptor = MakeRGBADescriptor(fd);
  auto& image = descriptor.get_SurfaceDescriptorDMABuf();
  image.fence().AppendElement(WrapNotNull(fd));
  image.refCount().AppendElement(ipc::FileDescriptor(fd->GetHandle()));
  image.foreignRGBImageState() = Some(ForeignRGBImageState(1, WrapNotNull(fd)));
  RefPtr<DMABufSurface> rejected =
      DMABufSurface::CreateDMABufSurface(descriptor);
  EXPECT_FALSE(rejected);
}
#endif

#ifdef XP_LINUX
TEST(DMABufSurface, ForeignRGBAccessLockIsSharedAndAbandonmentPersists)
{
  auto fd = MakeFd();
  ASSERT_TRUE(fd);
  int rawLock = syscall(SYS_memfd_create, "foreign-rgb-test",
                        MFD_CLOEXEC | MFD_ALLOW_SEALING);
  ASSERT_GE(rawLock, 0);
  RefPtr<FileHandleWrapper> lock =
      new FileHandleWrapper(UniqueFileHandle(rawLock));
  ASSERT_EQ(ftruncate(rawLock, sizeof(uint32_t)), 0);
  ASSERT_EQ(
      fcntl(rawLock, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL),
      0);
  int rawRef = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE);
  ASSERT_GE(rawRef, 0);
  RefPtr<FileHandleWrapper> refs =
      new FileHandleWrapper(UniqueFileHandle(rawRef));
  auto descriptor = MakeRGBADescriptor(fd);
  auto& image = descriptor.get_SurfaceDescriptorDMABuf();
  image.fence().AppendElement(WrapNotNull(fd));
  image.refCount().AppendElement(ipc::FileDescriptor(rawRef));
  image.foreignRGBImageState() =
      Some(ForeignRGBImageState(1, WrapNotNull(lock)));
  RefPtr<DMABufSurface> first = DMABufSurface::CreateDMABufSurface(descriptor);
  RefPtr<DMABufSurface> second = DMABufSurface::CreateDMABufSurface(descriptor);
  ASSERT_TRUE(first);
  ASSERT_TRUE(second);
  ASSERT_TRUE(first->LockForeignRGB());
  first->UnlockForeignRGB();
  ASSERT_TRUE(second->LockForeignRGB());
  second->UnlockForeignRGB(true);
  EXPECT_FALSE(first->ForeignRGBUsable());
  EXPECT_FALSE(first->LockForeignRGB());
  first->UnlockForeignRGB();
  EXPECT_FALSE(second->ForeignRGBUsable());
}
#endif
