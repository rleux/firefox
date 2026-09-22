/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <fcntl.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "FFmpegVideoFramePool.h"
#include "base/linux_memfd_defs.h"
#include "gtest/gtest.h"
#include "mozilla/gfx/FileHandleWrapper.h"
#include "mozilla/layers/DMABUFTextureClientOGL.h"
#include "mozilla/layers/LayersSurfaces.h"

using namespace mozilla;
using namespace mozilla::gfx;
using namespace mozilla::layers;

namespace {

struct BufferCount {
  int references = 1;
  bool fail = false;
};

AVBufferRef* RetainBuffer(AVBufferRef* aBuffer) {
  auto* count = reinterpret_cast<BufferCount*>(aBuffer->data);
  if (count->fail) return nullptr;
  ++count->references;
  return new AVBufferRef(*aBuffer);
}

void ReleaseBuffer(AVBufferRef** aBuffer) {
  if (!*aBuffer) return;
  auto* count = reinterpret_cast<BufferCount*>((*aBuffer)->data);
  --count->references;
  delete *aBuffer;
  *aBuffer = nullptr;
}

FFmpegLibWrapper Library() {
  FFmpegLibWrapper lib{};
  lib.av_buffer_ref = RetainBuffer;
  lib.av_buffer_unref = ReleaseBuffer;
  return lib;
}

struct Input {
  BufferCount contextCount;
  BufferCount frameCount;
  AVBufferRef context{};
  AVBufferRef buffer{};
  AVFrame frame{};
  VADRMPRIMESurfaceDescriptor descriptor{};
  RefPtr<FileHandleWrapper> memory;

  bool Init() {
    const int fd =
        syscall(SYS_memfd_create, "native-vaapi-pool-test", MFD_CLOEXEC);
    if (fd < 0) return false;
    memory = new FileHandleWrapper(UniqueFileHandle(fd));
    if (ftruncate(fd, 24576)) return false;
    context.data = reinterpret_cast<uint8_t*>(&contextCount);
    buffer.data = reinterpret_cast<uint8_t*>(&frameCount);
    frame.hw_frames_ctx = &context;
    frame.buf[0] = &buffer;
    descriptor.fourcc = VA_FOURCC_NV12;
    descriptor.width = descriptor.height = 128;
    descriptor.num_objects = 1;
    descriptor.objects[0].fd = fd;
    descriptor.objects[0].size = 24576;
    descriptor.objects[0].drm_format_modifier = 0;
    descriptor.num_layers = 2;
    for (size_t i = 0; i < 2; ++i) {
      descriptor.layers[i].drm_format = i ? GBM_FORMAT_GR88 : GBM_FORMAT_R8;
      descriptor.layers[i].num_planes = 1;
      descriptor.layers[i].object_index[0] = 0;
      descriptor.layers[i].offset[0] = i ? 16384 : 0;
      descriptor.layers[i].pitch[0] = 128;
    }
    return true;
  }

  RefPtr<VideoFrameSurface<LIBAV_VER>> Publish(VideoFramePool<LIBAV_VER>& aPool,
                                               const FFmpegLibWrapper& aLib) {
    return aPool.GetNativeVAAPIFrame(
        DMABufSurfaceYUV::CreateYUVSurface(descriptor, 128, 128), descriptor,
        &frame, &aLib, 226, 128);
  }
};

}  // namespace

TEST(VAAPIFramePool, RetainsCallerAndImageUntilPublicationRetires)
{
  Input input;
  ASSERT_TRUE(input.Init());
  auto lib = Library();
  VideoFramePool<LIBAV_VER> pool(16, true);
  auto frame = input.Publish(pool, lib);
  ASSERT_TRUE(frame);
  EXPECT_EQ(input.frameCount.references, 2);
  EXPECT_EQ(input.contextCount.references, 2);
  pool.ReleaseUnusedVAAPIFrames();
  EXPECT_EQ(input.frameCount.references, 2);
  auto image = frame->GetAsImage();
  SurfaceDescriptor descriptor;
  ASSERT_TRUE(frame->GetDMABufSurface()->Serialize(descriptor));
  frame = nullptr;
  pool.ReleaseUnusedVAAPIFrames();
  EXPECT_EQ(input.frameCount.references, 2);
  image = nullptr;
  pool.ReleaseUnusedVAAPIFrames();
  EXPECT_EQ(input.frameCount.references, 1);
  EXPECT_EQ(input.contextCount.references, 1);
  RefPtr<DMABufSurface> late = DMABufSurface::CreateDMABufSurface(descriptor);
  EXPECT_FALSE(late);
}

TEST(VAAPIFramePool, TextureDataPinsPublicationUntilEveryCleanupPath)
{
  enum class Cleanup { Destructor, Deallocate, Forget };
  for (const auto cleanup :
       {Cleanup::Destructor, Cleanup::Deallocate, Cleanup::Forget}) {
    SCOPED_TRACE(static_cast<int>(cleanup));
    Input input;
    ASSERT_TRUE(input.Init());
    auto lib = Library();
    VideoFramePool<LIBAV_VER> pool(16, true);
    auto frame = input.Publish(pool, lib);
    ASSERT_TRUE(frame);
    auto surface = frame->GetDMABufSurface();
    auto image = frame->GetAsImage();
    UniquePtr<DMABUFTextureData> texture(
        DMABUFTextureData::Create(surface, gfx::BackendType::SKIA));
    SurfaceDescriptor descriptor;
    ASSERT_TRUE(texture->Serialize(descriptor));

    frame = nullptr;
    image = nullptr;
    pool.ReleaseUnusedVAAPIFrames();
    EXPECT_EQ(input.frameCount.references, 2);
    EXPECT_EQ(input.contextCount.references, 2);
    RefPtr<DMABufSurface> consumer =
        DMABufSurface::CreateDMABufSurface(descriptor);
    ASSERT_TRUE(consumer);
    consumer = nullptr;

    switch (cleanup) {
      case Cleanup::Destructor:
        texture = nullptr;
        break;
      case Cleanup::Deallocate:
        texture->Deallocate(nullptr);
        break;
      case Cleanup::Forget:
        texture->Forget(nullptr);
        break;
    }
    pool.ReleaseUnusedVAAPIFrames();
    EXPECT_EQ(input.frameCount.references, 1);
    EXPECT_EQ(input.contextCount.references, 1);
    texture = nullptr;
    pool.ReleaseUnusedVAAPIFrames();
    EXPECT_EQ(input.frameCount.references, 1);
    EXPECT_EQ(input.contextCount.references, 1);
    RefPtr<DMABufSurface> late = DMABufSurface::CreateDMABufSurface(descriptor);
    EXPECT_FALSE(late);
  }
}

TEST(VAAPIFramePool, RepeatedOutputSharesPublicationAcrossFlush)
{
  Input input;
  ASSERT_TRUE(input.Init());
  auto lib = Library();
  VideoFramePool<LIBAV_VER> pool(16, true);
  auto first = input.Publish(pool, lib);
  ASSERT_TRUE(first);
  const auto firstState =
      first->GetDMABufSurface()->GetVAAPIDescriptor()->vaapiImageState().ref();
  auto second = input.Publish(pool, lib);
  ASSERT_TRUE(second);
  EXPECT_EQ(first.get(), second.get());
  EXPECT_EQ(input.frameCount.references, 2);
  pool.FlushFFmpegFrames();
  auto replay = input.Publish(pool, lib);
  ASSERT_EQ(first.get(), replay.get());
  EXPECT_EQ(replay->GetDMABufSurface()
                ->GetVAAPIDescriptor()
                ->vaapiImageState()
                ->generation(),
            firstState.generation());
  replay = nullptr;
  second = nullptr;
  first = nullptr;
  pool.ReleaseUnusedVAAPIFrames();
  EXPECT_EQ(input.frameCount.references, 1);
  auto next = input.Publish(pool, lib);
  ASSERT_TRUE(next);
  const auto& state =
      next->GetDMABufSurface()->GetVAAPIDescriptor()->vaapiImageState().ref();
  EXPECT_NE(state.generation(), firstState.generation());
  EXPECT_NE(state.producerEpoch(), firstState.producerEpoch());
}

TEST(VAAPIFramePool, BusyAccessPreventsRetirement)
{
  Input input;
  ASSERT_TRUE(input.Init());
  auto lib = Library();
  VideoFramePool<LIBAV_VER> pool(16, true);
  auto frame = input.Publish(pool, lib);
  ASSERT_TRUE(frame);
  auto surface = frame->GetDMABufSurface();
  frame = nullptr;
  ASSERT_TRUE(surface->TryLockAccess());
  pool.ReleaseUnusedVAAPIFrames();
  EXPECT_EQ(input.frameCount.references, 2);
  surface->UnlockAccess();
  pool.ReleaseUnusedVAAPIFrames();
  EXPECT_EQ(input.frameCount.references, 1);
  EXPECT_FALSE(surface->TryLockAccess());
}

TEST(VAAPIFramePool, PressureFailsWithoutDroppingRetainedFrames)
{
  Input firstInput, secondInput;
  ASSERT_TRUE(firstInput.Init());
  ASSERT_TRUE(secondInput.Init());
  auto lib = Library();
  {
    VideoFramePool<LIBAV_VER> pool(2, true);
    auto first = firstInput.Publish(pool, lib);
    ASSERT_TRUE(first);
    auto image = first->GetAsImage();
    first = nullptr;
    EXPECT_FALSE(secondInput.Publish(pool, lib));
    EXPECT_EQ(secondInput.frameCount.references, 1);
    image = nullptr;
    pool.ReleaseUnusedVAAPIFrames();
    pool.FlushFFmpegFrames();
    EXPECT_EQ(firstInput.frameCount.references, 2);
    EXPECT_FALSE(secondInput.Publish(pool, lib));
  }
  EXPECT_EQ(firstInput.frameCount.references, 1);
  EXPECT_EQ(firstInput.contextCount.references, 1);
}

TEST(VAAPIFramePool, AbandonmentQuarantinesUntilPoolShutdown)
{
  Input input;
  ASSERT_TRUE(input.Init());
  auto lib = Library();
  {
    VideoFramePool<LIBAV_VER> pool(16, true);
    auto frame = input.Publish(pool, lib);
    ASSERT_TRUE(frame);
    SurfaceDescriptor descriptor;
    ASSERT_TRUE(frame->GetDMABufSurface()->Serialize(descriptor));
    RefPtr<DMABufSurface> consumer =
        DMABufSurface::CreateDMABufSurface(descriptor);
    ASSERT_TRUE(consumer);
    ASSERT_TRUE(consumer->TryLockAccess());
    consumer->UnlockAccess(true);
    consumer = nullptr;
    frame = nullptr;
    pool.ReleaseUnusedVAAPIFrames();
    EXPECT_EQ(input.frameCount.references, 2);
    EXPECT_FALSE(input.Publish(pool, lib));
  }
  EXPECT_EQ(input.frameCount.references, 1);
}

TEST(VAAPIFramePool, RejectsMetadataChangeForLiveAllocation)
{
  Input input;
  ASSERT_TRUE(input.Init());
  auto lib = Library();
  VideoFramePool<LIBAV_VER> pool(16, true);
  auto frame = input.Publish(pool, lib);
  ASSERT_TRUE(frame);
  RefPtr<DMABufSurfaceYUV> changed =
      DMABufSurfaceYUV::CreateYUVSurface(input.descriptor, 128, 128);
  ASSERT_TRUE(changed);
  changed->SetColorRange(ColorRange::FULL);
  EXPECT_FALSE(pool.GetNativeVAAPIFrame(changed, input.descriptor, &input.frame,
                                        &lib, 226, 128));
  EXPECT_FALSE(frame->GetDMABufSurface()->IsFullRange());
  EXPECT_EQ(input.frameCount.references, 2);
}

TEST(VAAPIFramePool, FailedBufferPinReleasesPartialReferences)
{
  for (bool context : {false, true}) {
    Input input;
    ASSERT_TRUE(input.Init());
    auto lib = Library();
    VideoFramePool<LIBAV_VER> pool(16, true);
    (context ? input.contextCount : input.frameCount).fail = true;
    EXPECT_FALSE(input.Publish(pool, lib));
    EXPECT_EQ(input.frameCount.references, 1);
    EXPECT_EQ(input.contextCount.references, 1);
  }
}
