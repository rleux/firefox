/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <atomic>
#include <memory>

#include "gfxPlatform.h"
#include "gtest/gtest.h"
#include "mozilla/TimeStamp.h"
#include "mozilla/layers/SynchronousTask.h"
#include "mozilla/layers/VulkanImages.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "mozilla/webrender/RenderTextureHostWrapper.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/webrender/RenderVulkanDMABufTextureHost.h"
#include "mozilla/webrender/VulkanImageIPC.h"
#include "mozilla/webrender/VulkanImageRetirement.h"
#include "mozilla/widget/CompositorWidget.h"
#include "nsIThread.h"
#include "nsThreadUtils.h"
#include "prthread.h"

#include <array>
#include <cstring>
#include <vector>

#ifdef MOZ_WIDGET_GTK
#include "GLContextProvider.h"
#include "GLScreenBuffer.h"
#include "MozFramebuffer.h"
#include "ScopedGLHelpers.h"
#include "SharedSurfaceDMABUF.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/gfxVars.h"
#include "mozilla/layers/LayersSurfaces.h"
#include "mozilla/webrender/RenderDMABUFTextureHost.h"

namespace mozilla {
void InitSwapChain(gl::GLContext &, gl::SwapChain &, layers::TextureType, bool);
}
#endif


namespace mozilla::wr {
class VulkanNativeImageTestAccess {
 public:
  static void ToggleDriverIdentity(RenderVulkanDMABufTextureHost* aHost) {
    aHost->mDescriptor.driver_uuid[0] ^= 0xff;
  }
  static WrVulkanRelease* CurrentRelease(WrVulkanDmaBufImage* aImage,
                                         bool aBorrowed, uint64_t aLastIssued) {
    return RenderVulkanDMABufTextureHost::CurrentRelease(aImage, aBorrowed,
                                                         aLastIssued);
  }
};
namespace {
struct TestNativeImage {
  int32_t memory_fd;
  int32_t ready_fd;
  uint64_t offset;
  uint64_t stride;
  uint8_t device_uuid[16];
  uint8_t driver_uuid[16];
};
extern "C" {
struct TestLock {
  uint64_t handle;
  uint32_t state;
};
void wr_test_native_publish(void*, void*, TestLock (*)(void*), void (*)(void*));
void wr_test_native_open_ready(void*);
void* wr_test_native_new(TestNativeImage*);
void* wr_test_native_new_alpha(TestNativeImage*, uint8_t);
Renderer* wr_test_native_renderer(void*);
void wr_test_native_sample(void*, ExternalTextureHandle);
void wr_test_native_republish(void*, int32_t, uint64_t);
void wr_test_native_drop_renderer(void*);
void wr_test_native_replace_renderer(void*);
void wr_test_native_delete(void*);
}
class VulkanNativeImages : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!getenv("WR_NATIVE_IMAGE_TEST")) {
      GTEST_SKIP()
          << "Requires native Vulkan DMA-BUF and modern sync validation";
    }
    gfxPlatform::GetPlatform();
    gfxPlatform::InitLayersIPC();
    ASSERT_TRUE(RenderThread::Get());
  }
  template <typename F>
  void OnRenderThread(F&& aTest) {
    layers::SynchronousTask task("VulkanNativeImages");
    nsCOMPtr<nsIThread> thread = RenderThread::GetRenderThread();
    ASSERT_EQ(NS_OK,
              thread->Dispatch(NS_NewRunnableFunction(
                                   "VulkanNativeImages",
                                   [test = std::move(aTest), &task] {
                                     layers::AutoCompleteTask complete(&task);
                                     test();
                                   }),
                               NS_DISPATCH_NORMAL));
    task.Wait();
  }
};
struct ReturnState {
  VulkanImageReturn value;
  std::atomic<bool> done{false};
  std::atomic<bool> dispatched{false};
};
struct Fixture {
  void* native = nullptr;
  WrVulkanDmaBufDescriptor descriptor{};
  WrVulkanTimelineDescriptor ready{};
  UniquePtr<RenderCompositorVulkan> compositor;
  RefPtr<RenderTextureHost> host;
  RenderVulkanDMABufTextureHost* nativeHost = nullptr;
  std::shared_ptr<ReturnState> completion = std::make_shared<ReturnState>();
};
void Create(Fixture& aFixture, uint64_t aReadyValue = 1, bool aIPC = false,
            uint8_t aAlpha = 255) {
  TestNativeImage source{};
  aFixture.native = wr_test_native_new_alpha(&source, aAlpha);
  WrVulkanDmaBufDescriptor image{};
  image.fd = source.memory_fd;
  image.width = 4;
  image.height = 4;
  image.format = ImageFormat::RGBA8;
  image.offset = source.offset;
  image.stride = source.stride;
  image.copy_src = true;
  image.copy_dst = true;
  WrVulkanTimelineDescriptor ready{};
  ready.fd = source.ready_fd;
  std::copy(std::begin(source.device_uuid), std::end(source.device_uuid),
            image.device_uuid);
  std::copy(std::begin(source.driver_uuid), std::end(source.driver_uuid),
            image.driver_uuid);
  std::copy(std::begin(source.device_uuid), std::end(source.device_uuid),
            ready.device_uuid);
  std::copy(std::begin(source.driver_uuid), std::end(source.driver_uuid),
            ready.driver_uuid);
  aFixture.descriptor = image;
  aFixture.ready = ready;
  if (aIPC) {
    layers::VulkanImagePublication publication;
    publication.publicationId() = 55;
    publication.memory() =
        new gfx::FileHandleWrapper(DuplicateFileHandle(image.fd));
    publication.size() = gfx::IntSize(4, 4);
    publication.format() = gfx::SurfaceFormat::R8G8B8A8;
    publication.modifier() = 0;
    publication.offset() = image.offset;
    publication.stride() = image.stride;
    publication.copySrc() = true;
    publication.copyDst() = true;
    publication.colorTarget() = false;
    publication.ready().handle() =
        new gfx::FileHandleWrapper(DuplicateFileHandle(ready.fd));
    publication.ready().value() = aReadyValue;
    std::copy(std::begin(image.device_uuid), std::end(image.device_uuid),
              publication.ready().deviceUUID().begin());
    std::copy(std::begin(image.driver_uuid), std::end(image.driver_uuid),
              publication.ready().driverUUID().begin());
    aFixture.host = CreateVulkanImageHost(
        publication, [state = aFixture.completion,
                      publication](layers::VulkanImageReturnMessage&& result) {
          EXPECT_TRUE(RenderThread::IsInRenderThread());
          EXPECT_TRUE(ValidateVulkanImageReturn(result, publication));
          state->value.mStatus = result.status();
          if (result.signal()) {
            state->value.mSemaphore =
                DuplicateFileHandle(result.signal()->handle()->GetHandle());
            state->value.mValue = result.signal()->value();
            state->value.mDeviceUUID = result.signal()->deviceUUID();
            state->value.mDriverUUID = result.signal()->driverUUID();
          }
          state->dispatched.store(true, std::memory_order_release);
          state->done.store(true, std::memory_order_release);
        });
  } else {
    aFixture.host = RenderVulkanDMABufTextureHost::Create(
        image, ready, aReadyValue,
        [state = aFixture.completion](VulkanImageReturn&& result) {
          state->value = std::move(result);
          state->done.store(true, std::memory_order_release);
        });
  }
  ASSERT_TRUE(aFixture.host);
  aFixture.nativeHost =
      static_cast<RenderVulkanDMABufTextureHost*>(aFixture.host.get());
  ExternalImageId id{0x7fff000000000001};
  RefPtr<RenderTextureHost> original = aFixture.host;
  RenderThread::Get()->RegisterExternalImage(id, original.forget());
  aFixture.host = new RenderTextureHostWrapper(id);
  RenderThread::Get()->UnregisterExternalImage(id);
  EXPECT_EQ(aFixture.host->GetFormat(), gfx::SurfaceFormat::R8G8B8A8);
  WrVulkanConfig config{};
  config.validation = true;
  aFixture.compositor =
      std::make_unique<RenderCompositorVulkan>(nullptr, config);
  aFixture.compositor->SetRenderer(wr_test_native_renderer(aFixture.native),
                                   WindowId{0});
}
void SampleAndRelease(Fixture& aFixture) {
  auto& compositor = *aFixture.compositor;
  size_t pending = 0;
  const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
  for (;;) {
    auto image = compositor.LockExternalImage(aFixture.host, 0);
    if (image.image_type == WrExternalImageType::NativeTexture) {
      wr_test_native_sample(aFixture.native,
                            ExternalTextureHandle{image.handle});
      auto opaque = compositor.LockExternalImage(aFixture.host, 1);
      ASSERT_EQ(opaque.image_type, WrExternalImageType::NativeTexture);
      ASSERT_NE(opaque.handle, image.handle);
      wr_test_native_sample(aFixture.native,
                            ExternalTextureHandle{opaque.handle});
      compositor.UnlockExternalImage(aFixture.host);
      compositor.UnlockExternalImage(aFixture.host);
      break;
    }
    ASSERT_EQ(image.image_type, WrExternalImageType::Pending);
    ++pending;
    compositor.UnlockExternalImage(aFixture.host);
    compositor.AfterRender(false);
    ASSERT_LT(TimeStamp::Now(), deadline);
  }
  EXPECT_GT(pending, 0U);
  compositor.AfterRender(true);
}
TestLock LockFixture(void* aContext) {
  auto& fixture = *static_cast<Fixture*>(aContext);
  auto image = fixture.compositor->LockExternalImage(fixture.host, 0);
  return {image.handle, image.image_type == WrExternalImageType::Pending ? 0U
                        : image.image_type == WrExternalImageType::NativeTexture
                            ? 1U
                            : 2U};
}
void UnlockFixture(void* aContext) {
  auto& fixture = *static_cast<Fixture*>(aContext);
  fixture.compositor->UnlockExternalImage(fixture.host);
}
}  // namespace

TEST_F(VulkanNativeImages, SnapshotPrefersImportedRendererThroughWrapper) {
  Fixture owner;
  Fixture unrelated;
  OnRenderThread([&] {
    Create(owner);
    Create(unrelated);
    std::array<uint8_t, 64> pixels{};
    ASSERT_TRUE(owner.compositor->ReadbackNativeImage(owner.host, pixels.data(),
                                                      pixels.size()));
    EXPECT_TRUE(owner.compositor->CanReadbackNativeImage(owner.host, false));
    EXPECT_FALSE(
        unrelated.compositor->CanReadbackNativeImage(owner.host, false));
    EXPECT_TRUE(unrelated.compositor->CanReadbackNativeImage(owner.host, true));
    // A mismatched physical/driver identity must be rejected before LockVulkan
    // can switch or permanently fail the still-displayed publication.
    VulkanNativeImageTestAccess::ToggleDriverIdentity(owner.nativeHost);
    EXPECT_FALSE(
        unrelated.compositor->CanReadbackNativeImage(owner.host, true));
    EXPECT_FALSE(unrelated.compositor->ReadbackNativeImage(
        owner.host, pixels.data(), pixels.size()));
    VulkanNativeImageTestAccess::ToggleDriverIdentity(owner.nativeHost);
    EXPECT_TRUE(owner.compositor->CanReadbackNativeImage(owner.host, false));
    RenderCompositor* candidates[] = {unrelated.compositor.get(),
                                      owner.compositor.get()};
    ASSERT_TRUE(RenderThread::ReadbackNativeImage(
        Span<RenderCompositor*>(candidates), owner.host, pixels.data(),
        pixels.size(), false));
    EXPECT_TRUE(owner.compositor->CanReadbackNativeImage(owner.host, false));
    EXPECT_FALSE(
        unrelated.compositor->CanReadbackNativeImage(owner.host, false));
    // Removing the original renderer permits a compatible migration, using
    // exactly the selector called by SnapshotNativeImage.
    wr_test_native_drop_renderer(owner.native);
    owner.compositor->SetRenderer(nullptr, WindowId{0});
    RenderCompositor* replacement[] = {unrelated.compositor.get()};
    ASSERT_TRUE(RenderThread::ReadbackNativeImage(
        Span<RenderCompositor*>(replacement), owner.host, pixels.data(),
        pixels.size(), false));
    EXPECT_TRUE(
        unrelated.compositor->CanReadbackNativeImage(owner.host, false));
    for (size_t i = 0; i < pixels.size(); i += 4) {
      EXPECT_EQ(pixels[i], 255);
      EXPECT_EQ(pixels[i + 1], 0);
      EXPECT_EQ(pixels[i + 2], 0);
      EXPECT_EQ(pixels[i + 3], 255);
    }
    owner.host = nullptr;
    unrelated.host = nullptr;
    wr_test_native_drop_renderer(unrelated.native);
    unrelated.compositor->SetRenderer(nullptr, WindowId{0});
    owner.compositor = nullptr;
    unrelated.compositor = nullptr;
  });
  for (auto* fixture : {&owner, &unrelated}) {
    const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
    while (!fixture->completion->done.load(std::memory_order_acquire) &&
           TimeStamp::Now() < deadline) {
      PR_Sleep(PR_MillisecondsToInterval(1));
    }
    EXPECT_TRUE(fixture->completion->done.load(std::memory_order_acquire));
    OnRenderThread([&] { wr_test_native_delete(fixture->native); });
  }
}

TEST_F(VulkanNativeImages, OpaqueSnapshotThroughWrapperPreservesSourceAlpha) {
  for (uint8_t alpha : {uint8_t(0), uint8_t(128)}) {
    Fixture fixture;
    OnRenderThread([&] {
      Create(fixture, 1, false, alpha);
      std::array<uint8_t, 64> pixels{};
      ASSERT_TRUE(fixture.compositor->ReadbackNativeImage(
          fixture.host, pixels.data(), pixels.size(), true));
      for (size_t i = 0; i < pixels.size(); i += 4) {
        EXPECT_EQ(pixels[i], 255);
        EXPECT_EQ(pixels[i + 1], 0);
        EXPECT_EQ(pixels[i + 2], 0);
        EXPECT_EQ(pixels[i + 3], 255);
      }
      ASSERT_TRUE(fixture.compositor->ReadbackNativeImage(
          fixture.host, pixels.data(), pixels.size(), false));
      for (size_t i = 3; i < pixels.size(); i += 4) {
        EXPECT_EQ(pixels[i], alpha);
      }
      wr_test_native_drop_renderer(fixture.native);
      fixture.compositor->SetRenderer(nullptr, WindowId{0});
      fixture.compositor = nullptr;
      fixture.host = nullptr;
    });
    const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
    while (!fixture.completion->done.load(std::memory_order_acquire) &&
           TimeStamp::Now() < deadline) {
      PR_Sleep(PR_MillisecondsToInterval(1));
    }
    EXPECT_TRUE(fixture.completion->done.load(std::memory_order_acquire));
    EXPECT_EQ(fixture.completion->value.mStatus,
              VulkanImageReturnStatus::Submitted);
    OnRenderThread([&] { wr_test_native_delete(fixture.native); });
  }
}

TEST_F(VulkanNativeImages,
       CanvasSnapshotThroughWrapperSurvivesRendererMigration) {
  Fixture fixture;
  OnRenderThread([&] {
    Create(fixture);
    for (unsigned iteration = 0; iteration < 2; ++iteration) {
      std::array<uint8_t, 64> pixels{};
      ASSERT_TRUE(fixture.compositor->ReadbackNativeImage(
          fixture.host, pixels.data(), pixels.size()));
      for (size_t i = 0; i < pixels.size(); i += 4) {
        EXPECT_EQ(pixels[i], 255);
        EXPECT_EQ(pixels[i + 1], 0);
        EXPECT_EQ(pixels[i + 2], 0);
        EXPECT_EQ(pixels[i + 3], 255);
      }
      EXPECT_FALSE(fixture.compositor->ReadbackNativeImage(
          fixture.host, pixels.data(), pixels.size() - 1));
      wr_test_native_drop_renderer(fixture.native);
      fixture.compositor->SetRenderer(nullptr, WindowId{0});
      fixture.compositor = nullptr;
      if (iteration == 0) {
        wr_test_native_replace_renderer(fixture.native);
        WrVulkanConfig config{};
        config.validation = true;
        fixture.compositor =
            std::make_unique<RenderCompositorVulkan>(nullptr, config);
        fixture.compositor->SetRenderer(wr_test_native_renderer(fixture.native),
                                        WindowId{0});
      }
    }
    fixture.host = nullptr;
  });
  const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
  while (!fixture.completion->done.load(std::memory_order_acquire) &&
         TimeStamp::Now() < deadline) {
    PR_Sleep(PR_MillisecondsToInterval(1));
  }
  EXPECT_TRUE(fixture.completion->done.load(std::memory_order_acquire));
  EXPECT_EQ(fixture.completion->value.mStatus,
            VulkanImageReturnStatus::Submitted);
  OnRenderThread([&] {
    wr_test_native_delete(fixture.native);
    fixture.native = nullptr;
  });
}

TEST_F(VulkanNativeImages, IdleReceiptAndHostReturnSurviveRendererDrop) {
  Fixture fixture;
  OnRenderThread([&] {
    Create(fixture, 1, true);
    SampleAndRelease(fixture);
    wr_test_native_drop_renderer(fixture.native);
    fixture.compositor->SetRenderer(nullptr, WindowId{0});
    fixture.compositor = nullptr;
    fixture.host = nullptr;
  });
  const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
  while (!fixture.completion->done.load(std::memory_order_acquire) &&
         TimeStamp::Now() < deadline) {
    PR_Sleep(PR_MillisecondsToInterval(1));
  }
  EXPECT_TRUE(fixture.completion->done.load(std::memory_order_acquire));
  EXPECT_TRUE(fixture.completion->dispatched.load(std::memory_order_acquire));
  EXPECT_EQ(fixture.completion->value.mStatus,
            VulkanImageReturnStatus::Submitted);
  EXPECT_GT(fixture.completion->value.mValue, 0U);
  EXPECT_TRUE(fixture.completion->value.mSemaphore);
  OnRenderThread([&] {
    wr_test_native_republish(fixture.native,
                             fixture.completion->value.mSemaphore.get(),
                             fixture.completion->value.mValue);
    wr_test_native_delete(fixture.native);
    fixture.native = nullptr;
  });
}

TEST_F(VulkanNativeImages, PublicationMigratesWithoutReturningProducerEarly) {
  Fixture fixture;
  OnRenderThread([&] {
    Create(fixture);
    SampleAndRelease(fixture);
    RenderThread::Get()->NativeImages().Poll();
    wr_test_native_drop_renderer(fixture.native);
    fixture.compositor->SetRenderer(nullptr, WindowId{0});
    fixture.compositor = nullptr;
    EXPECT_FALSE(fixture.completion->done.load(std::memory_order_acquire));
    wr_test_native_replace_renderer(fixture.native);
    WrVulkanConfig config{};
    config.validation = true;
    fixture.compositor =
        std::make_unique<RenderCompositorVulkan>(nullptr, config);
    fixture.compositor->SetRenderer(wr_test_native_renderer(fixture.native),
                                    WindowId{0});
    SampleAndRelease(fixture);
    EXPECT_FALSE(fixture.completion->done.load(std::memory_order_acquire));
    wr_test_native_drop_renderer(fixture.native);
    fixture.compositor->SetRenderer(nullptr, WindowId{0});
    fixture.compositor = nullptr;
    fixture.host = nullptr;
  });
  const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
  while (!fixture.completion->done.load(std::memory_order_acquire) &&
         TimeStamp::Now() < deadline) {
    PR_Sleep(PR_MillisecondsToInterval(1));
  }
  EXPECT_TRUE(fixture.completion->done.load(std::memory_order_acquire));
  EXPECT_EQ(fixture.completion->value.mStatus,
            VulkanImageReturnStatus::Submitted);
  OnRenderThread([&] {
    wr_test_native_delete(fixture.native);
    fixture.native = nullptr;
  });
}

TEST_F(VulkanNativeImages, PendingFfiWithholdsEpochsWithoutReportingFailure) {
  Fixture fixture;
  OnRenderThread([&] {
    Create(fixture, 2);
    auto* renderer = wr_test_native_renderer(fixture.native);
    wr_test_native_publish(fixture.native, &fixture, LockFixture,
                           UnlockFixture);
    auto update = WrRendererUpdateStatus::Ready;
    ASSERT_TRUE(wr_renderer_update(renderer, &update));
    EXPECT_EQ(update, WrRendererUpdateStatus::Ready);
    RendererStats stats{};
    nsTArray<DeviceIntRect> dirty;
    bool rasterized = false, pending = false;
    WrPresentResult present{};
    for (size_t attempt = 0; attempt < 3; ++attempt) {
      ASSERT_TRUE(wr_renderer_render(renderer, 4, 4, 0, &stats, &dirty,
                                     &rasterized, &present, &pending));
      EXPECT_TRUE(pending);
      // Update is not FIFO-blocked, but this document still needs a real
      // present render; an update-only notifier must not retire its epochs.
      ASSERT_TRUE(wr_renderer_update(renderer, &update));
      EXPECT_EQ(update, WrRendererUpdateStatus::RenderPresent);
      EXPECT_EQ(present, WrPresentResult::Retry);
      EXPECT_TRUE(dirty.IsEmpty());
      WrPipelineInfo info;
      wr_renderer_flush_pipeline_info(renderer, &info);
      EXPECT_TRUE(info.epochs.IsEmpty());
      fixture.compositor->AfterRender(true);
    }
    wr_test_native_open_ready(fixture.native);
    const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
    do {
      ASSERT_TRUE(wr_renderer_render(renderer, 4, 4, 0, &stats, &dirty,
                                     &rasterized, &present, &pending));
      fixture.compositor->AfterRender(true);
      ASSERT_LT(TimeStamp::Now(), deadline);
    } while (pending);
    WrPipelineInfo info;
    wr_renderer_flush_pipeline_info(renderer, &info);
    EXPECT_FALSE(info.epochs.IsEmpty());
    wr_test_native_drop_renderer(fixture.native);
    fixture.compositor->SetRenderer(nullptr, WindowId{0});
    fixture.compositor = nullptr;
    fixture.host = nullptr;
  });
  const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
  while (!fixture.completion->done.load(std::memory_order_acquire) &&
         TimeStamp::Now() < deadline) {
    PR_Sleep(PR_MillisecondsToInterval(1));
  }
  EXPECT_TRUE(fixture.completion->done.load(std::memory_order_acquire));
  EXPECT_EQ(fixture.completion->value.mStatus,
            VulkanImageReturnStatus::Submitted);
  OnRenderThread([&] {
    wr_test_native_delete(fixture.native);
    fixture.native = nullptr;
  });
}

TEST_F(VulkanNativeImages, RejectedLockReturnsAbandonedWithoutReuseSignal) {
  Fixture fixture;
  OnRenderThread([&] {
    Create(fixture);
    EXPECT_EQ(fixture.compositor->LockExternalImage(fixture.host, 2).image_type,
              WrExternalImageType::Invalid);
    fixture.compositor->UnlockExternalImage(fixture.host);
    fixture.compositor->AfterRender(false);
    wr_test_native_drop_renderer(fixture.native);
    fixture.compositor->SetRenderer(nullptr, WindowId{0});
    fixture.compositor = nullptr;
    fixture.host = nullptr;
  });
  const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
  while (!fixture.completion->done.load(std::memory_order_acquire) &&
         TimeStamp::Now() < deadline) {
    PR_Sleep(PR_MillisecondsToInterval(1));
  }
  EXPECT_TRUE(fixture.completion->done.load(std::memory_order_acquire));
  EXPECT_EQ(fixture.completion->value.mStatus,
            VulkanImageReturnStatus::Abandoned);
  EXPECT_FALSE(fixture.completion->value.mSemaphore);
  EXPECT_EQ(fixture.completion->value.mValue, 0U);
  OnRenderThread([&] {
    wr_test_native_delete(fixture.native);
    fixture.native = nullptr;
  });
}

TEST_F(VulkanNativeImages, AbandonedNewBorrowCannotReturnAnOlderReceipt) {
  Fixture fixture;
  OnRenderThread([&] {
    Create(fixture);
    auto* context =
        wr_vulkan_external_images_new(wr_test_native_renderer(fixture.native));
    auto* image = wr_vulkan_dmabuf_import(context, &fixture.descriptor);
    auto* ready = wr_vulkan_timeline_import(context, &fixture.ready);
    ASSERT_TRUE(image);
    ASSERT_TRUE(ready);
    ExternalTextureHandle handle{};
    const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
    while (wr_vulkan_dmabuf_acquire(image, ready, 1, &handle) ==
           WrVulkanAcquireStatus::Pending) {
      wr_vulkan_process_native_work(context);
      ASSERT_LT(TimeStamp::Now(), deadline);
    }
    auto* older = wr_vulkan_dmabuf_release(image);
    ASSERT_TRUE(older);
    const auto value = wr_vulkan_release_value(older);
    EXPECT_EQ(wr_vulkan_release_status(older),
              WrVulkanReleaseStatus::Submitted);
    EXPECT_EQ(wr_vulkan_dmabuf_acquire(image, ready, 1, &handle),
              WrVulkanAcquireStatus::Ready);
    wr_vulkan_process_native_work(context);
    wr_vulkan_shutdown_native_work();
    EXPECT_EQ(wr_vulkan_release_status(older),
              WrVulkanReleaseStatus::Submitted);
    auto* stale =
        VulkanNativeImageTestAccess::CurrentRelease(image, true, value);
    EXPECT_EQ(stale, nullptr);
    wr_vulkan_release_delete(stale);
    EXPECT_EQ(wr_vulkan_dmabuf_acquire(image, ready, 1, &handle),
              WrVulkanAcquireStatus::Failed);
    wr_vulkan_release_delete(older);
    wr_vulkan_timeline_delete(ready);
    wr_vulkan_dmabuf_delete(image);
    wr_vulkan_external_images_delete(context);
    wr_test_native_drop_renderer(fixture.native);
    fixture.compositor->SetRenderer(nullptr, WindowId{0});
    fixture.compositor = nullptr;
    fixture.host = nullptr;
    RenderThread::Get()->NativeImages().Poll();
    EXPECT_TRUE(fixture.completion->done.load(std::memory_order_acquire));
    wr_test_native_delete(fixture.native);
    fixture.native = nullptr;
  });
}
#ifdef MOZ_WIDGET_GTK
extern "C" {
void *wr_test_foreign_new();
Renderer *wr_test_foreign_renderer(void *);
void wr_test_foreign_sample_expected(void *, ExternalTextureHandle, int32_t,
                                     int32_t, const uint8_t *, size_t);
void wr_test_foreign_delete(void *);
void *wr_test_foreign_replace(void *);
void wr_test_foreign_advance(void *);
bool wr_test_foreign_old_device_alive(void *);
}

class VulkanForeignImages : public VulkanNativeImages {
protected:
  void SetUp() override {
    VulkanNativeImages::SetUp();
    if (IsSkipped() || HasFatalFailure()) {
      return;
    }
    ASSERT_TRUE(NS_IsMainThread());
    const char *expected = getenv("WR_GL_FOREIGN_EXPECT_NATIVE");
    ASSERT_TRUE(expected && (!strcmp(expected, "0") || !strcmp(expected, "1")));
    mExpectNative = !strcmp(expected, "1");
    mOldVars = {gfx::gfxVars::UseWebRenderVulkan(),
                gfx::gfxVars::WebRenderVulkanX11Display(),
                gfx::gfxVars::UseSoftwareWebRender(), gfx::gfxVars::UseDMABuf(),
                gfx::gfxVars::UseDMABufWebGL()};
    mRestoreVars = true;
    gfx::gfxVars::SetUseWebRenderVulkan(true);
    gfx::gfxVars::SetWebRenderVulkanX11Display(true);
    gfx::gfxVars::SetUseSoftwareWebRender(false);
    gfx::gfxVars::SetUseDMABuf(true);
    gfx::gfxVars::SetUseDMABufWebGL(true);
    OnRenderThread([&] {
      mRender = wr_test_foreign_new();
      ASSERT_TRUE(mRender);
      WrVulkanConfig config{};
      config.validation = true;
      mFixture.compositor =
          std::make_unique<RenderCompositorVulkan>(nullptr, config);
      mFixture.compositor->SetRenderer(wr_test_foreign_renderer(mRender),
                                       WindowId{0});
    });
    gl::GLContextCreateDesc desc;
    desc.flags = gl::CreateContextFlags::PREFER_ES3 |
                 gl::CreateContextFlags::FORBID_SOFTWARE;
    nsAutoCString failure;
    mGL = gl::GLContextProviderEGL::CreateHeadless(desc, &failure);
    ASSERT_TRUE(mGL) << failure.get();
    ASSERT_TRUE(mGL->MakeCurrent());
  }

  void TearDown() override {
    if (mRender) {
      OnRenderThread([&] {
        mFixture.host = nullptr;
        mFixture.compositor->SetRenderer(nullptr, WindowId{0});
        mFixture.compositor = nullptr;
        wr_test_foreign_delete(mRender);
        mRender = nullptr;
        RenderThread::Get()->NativeImages().Poll();
      });
    }
    if (mGL) {
      mGL->MakeCurrent();
      mProducer = nullptr;
      mSwapChain = nullptr;
      mGL = nullptr;
    }
    if (mRestoreVars) {
      gfx::gfxVars::SetUseWebRenderVulkan(mOldVars[0]);
      gfx::gfxVars::SetWebRenderVulkanX11Display(mOldVars[1]);
      gfx::gfxVars::SetUseSoftwareWebRender(mOldVars[2]);
      gfx::gfxVars::SetUseDMABuf(mOldVars[3]);
      gfx::gfxVars::SetUseDMABufWebGL(mOldVars[4]);
    }
    VulkanNativeImages::TearDown();
  }

  enum class PublicationEnd { Reuse, Migrate, Abandon };
  void SamplePublication(const std::array<uint8_t, 4>& normal,
                         const std::array<uint8_t, 4>& opaque,
                         PublicationEnd end=PublicationEnd::Reuse) {
    auto descriptor = mProducer->ToSurfaceDescriptor();
    ASSERT_TRUE(descriptor);
    const auto &dma = descriptor->get_SurfaceDescriptorDMABuf();
    ASSERT_TRUE(dma.foreignRGBImageState());
    ASSERT_EQ(dma.fence().Length(), 1U);
    ASSERT_GE(dma.fence()[0]->GetHandle(), 0);
    const uint64_t generation = dma.foreignRGBImageState()->generation();
    EXPECT_GT(generation, mGeneration);
    mGeneration = generation;
    auto *surface = static_cast<gl::SharedSurface_DMABUF *>(mProducer.get());
    OnRenderThread([&] {
      RefPtr<DMABufSurface> imported =
          DMABufSurface::CreateDMABufSurface(*descriptor);
      ASSERT_TRUE(imported);
      ASSERT_TRUE(imported->GetForeignRGBDescriptor());
      RefPtr<RenderTextureHost> host = new RenderDMABUFTextureHost(imported);
      ExternalImageId id{0x7fff000000000002};
      RenderThread::Get()->RegisterExternalImage(id, host.forget());
      mFixture.host = new RenderTextureHostWrapper(id);
      RenderThread::Get()->UnregisterExternalImage(id);
    });
    ASSERT_TRUE(mFixture.host);
    ASSERT_TRUE(surface->mSurface->IsGlobalRefSet());
    EXPECT_FALSE(mProducer->IsValid());

    WrExternalImage image{};
    const auto deadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
    do {
      OnRenderThread([&] {
        image = mFixture.compositor->LockExternalImage(mFixture.host, 0);
        if (image.image_type != WrExternalImageType::NativeTexture) {
          mFixture.compositor->UnlockExternalImage(mFixture.host);
          mFixture.compositor->AfterRender(false);
        }
      });
      ASSERT_NE(image.image_type, WrExternalImageType::Invalid);
      if (image.image_type == WrExternalImageType::Pending) {
        ASSERT_LT(TimeStamp::Now(), deadline);
        PR_Sleep(PR_MillisecondsToInterval(1));
      }
    } while (image.image_type == WrExternalImageType::Pending);
    ASSERT_EQ(image.image_type, WrExternalImageType::NativeTexture);
    size_t locks = 1;
    auto unlock = MakeScopeExit([&] {
      if (locks) {
        OnRenderThread([&] {
          while (locks) {
            --locks;
            mFixture.compositor->UnlockExternalImage(mFixture.host);
          }
          mFixture.compositor->AfterRender(false);
        });
      }
    });
    ExpectForeignLocked(surface->mSurface);

    std::vector<uint8_t> expectedNormal, expectedOpaque;
    for (size_t pixel = 0; pixel < 17 * 9; ++pixel) {
      expectedNormal.insert(expectedNormal.end(), normal.begin(), normal.end());
      expectedOpaque.insert(expectedOpaque.end(), opaque.begin(), opaque.end());
    }
    OnRenderThread([&] {
      ++locks;
      auto opaqueImage=mFixture.compositor->LockExternalImage(mFixture.host,1);
      EXPECT_EQ(opaqueImage.image_type,WrExternalImageType::NativeTexture);
      if(opaqueImage.image_type==WrExternalImageType::NativeTexture) {
        EXPECT_NE(image.handle,opaqueImage.handle);
        wr_test_foreign_sample_expected(mRender,ExternalTextureHandle{image.handle},17,9,expectedNormal.data(),expectedNormal.size());
        wr_test_foreign_sample_expected(mRender,ExternalTextureHandle{opaqueImage.handle},17,9,expectedOpaque.data(),expectedOpaque.size());
      }
      if(end==PublicationEnd::Abandon){wr_vulkan_shutdown_native_work();}
      while(locks){--locks;mFixture.compositor->UnlockExternalImage(mFixture.host);}
      ExpectForeignLocked(surface->mSurface);
      if(end==PublicationEnd::Migrate) {
        mFixture.compositor->SetRenderer(nullptr,WindowId{0});
        mRender=wr_test_foreign_replace(mRender);
        mFixture.compositor->SetRenderer(wr_test_foreign_renderer(mRender),WindowId{0});
        wr_test_foreign_advance(mRender);
        EXPECT_TRUE(wr_test_foreign_old_device_alive(mRender));
        auto blocked=mFixture.compositor->LockExternalImage(mFixture.host,0);
        EXPECT_EQ(blocked.image_type,WrExternalImageType::Pending);
        mFixture.compositor->UnlockExternalImage(mFixture.host);
        ExpectForeignLocked(surface->mSurface);
      } else {
        if(end==PublicationEnd::Abandon) {
          EXPECT_FALSE(surface->mSurface->ForeignRGBUsable());
          EXPECT_FALSE(mProducer->IsValid());
        }
        mFixture.host=nullptr;
      }
      if(end==PublicationEnd::Abandon) {
        EXPECT_FALSE(surface->mSurface->IsGlobalRefSet());
        surface->mSurface->UnlockForeignRGB();
        EXPECT_FALSE(surface->mSurface->ForeignRGBUsable());
        EXPECT_FALSE(surface->mSurface->TryLockForeignRGB());
        EXPECT_FALSE(mProducer->IsValid());
      } else {
        EXPECT_TRUE(surface->mSurface->IsGlobalRefSet());
      }
      mFixture.compositor->AfterRender(false);
    });
    if(end==PublicationEnd::Migrate) {
      WrExternalImage migrated{};
      const auto migrationDeadline=TimeStamp::Now()+TimeDuration::FromSeconds(5);
      do {
        OnRenderThread([&]{
          migrated=mFixture.compositor->LockExternalImage(mFixture.host,0);
          if(migrated.image_type==WrExternalImageType::NativeTexture) {
            ++locks;
            auto opaqueImage=mFixture.compositor->LockExternalImage(mFixture.host,1);
            EXPECT_EQ(opaqueImage.image_type,WrExternalImageType::NativeTexture);
            if(opaqueImage.image_type==WrExternalImageType::NativeTexture) {
              wr_test_foreign_sample_expected(mRender,ExternalTextureHandle{migrated.handle},17,9,expectedNormal.data(),expectedNormal.size());
              wr_test_foreign_sample_expected(mRender,ExternalTextureHandle{opaqueImage.handle},17,9,expectedOpaque.data(),expectedOpaque.size());
            }
            mFixture.compositor->UnlockExternalImage(mFixture.host);--locks;
            EXPECT_FALSE(wr_test_foreign_old_device_alive(mRender));
          }
          mFixture.compositor->UnlockExternalImage(mFixture.host);
          if(migrated.image_type==WrExternalImageType::NativeTexture){mFixture.host=nullptr;}
          mFixture.compositor->AfterRender(false);
        });
        ASSERT_NE(migrated.image_type,WrExternalImageType::Invalid);
        if(migrated.image_type==WrExternalImageType::Pending){ASSERT_LT(TimeStamp::Now(),migrationDeadline);PR_Sleep(PR_MillisecondsToInterval(1));}
      } while(migrated.image_type==WrExternalImageType::Pending);
    }
    const auto returnDeadline = TimeStamp::Now() + TimeDuration::FromSeconds(5);
    while (surface->mSurface->IsGlobalRefSet() &&
           TimeStamp::Now() < returnDeadline) {
      PR_Sleep(PR_MillisecondsToInterval(1));
    }
    EXPECT_FALSE(surface->mSurface->IsGlobalRefSet());
    if(end==PublicationEnd::Abandon) {
      EXPECT_FALSE(surface->mSurface->ForeignRGBUsable());EXPECT_FALSE(surface->mSurface->TryLockForeignRGB());EXPECT_FALSE(mProducer->IsValid());
      return;
    }
    ASSERT_TRUE(surface->mSurface->ForeignRGBUsable());
    ASSERT_TRUE(surface->mSurface->TryLockForeignRGB());
    surface->mSurface->UnlockForeignRGB();
    ASSERT_TRUE(mProducer->IsValid());
  }

  static void ExpectForeignLocked(DMABufSurface *surface) {
    const bool acquired = surface->TryLockForeignRGB();
    EXPECT_FALSE(acquired);
    if (acquired) {
      surface->UnlockForeignRGB();
    }
  }

  bool mExpectNative = false;
  bool mRestoreVars = false;
  std::array<bool, 5> mOldVars{};
  Fixture mFixture;
  void *mRender = nullptr;
  RefPtr<gl::GLContext> mGL;
  UniquePtr<gl::SwapChain> mSwapChain;
  UniquePtr<gl::SharedSurface> mProducer;
  uint64_t mGeneration = 0;
};

TEST_F(VulkanForeignImages, FactoryInitializationAndCompletedProducerReuse) {
  mSwapChain = std::make_unique<gl::SwapChain>();
  InitSwapChain(*mGL, *mSwapChain, layers::TextureType::DMABUF, true);
  ASSERT_TRUE(mSwapChain->mFactory);
  if (!mExpectNative) {
    EXPECT_EQ(mSwapChain->mFactory->GetConsumerType(),
              layers::TextureType::Unknown);
    EXPECT_EQ(gl::SurfaceFactory_DMABUF::Create(*mGL), nullptr);
    auto basic = mSwapChain->mFactory->CreateShared(gfx::IntSize(17, 9),
                                                    gfx::ColorSpace2::SRGB);
    ASSERT_TRUE(basic);
    EXPECT_EQ(basic->mDesc.type, gl::SharedSurfaceType::Basic);
    return;
  }
  ASSERT_EQ(mSwapChain->mFactory->GetConsumerType(),
            layers::TextureType::DMABUF);
  {
    gl::ScopedGLState scissor(mGL, LOCAL_GL_SCISSOR_TEST, true);
    mGL->fScissor(1, 2, 1, 1);
    mGL->fColorMask(false, false, false, false);
    mGL->fClearColor(.25f, .5f, .75f, 1.f);
    Maybe<gl::ScopedGLState> discard;
    if (mGL->IsSupported(gl::GLFeature::transform_feedback2)) {
      discard.emplace(mGL, LOCAL_GL_RASTERIZER_DISCARD, true);
    }
    mProducer = mSwapChain->mFactory->CreateShared(gfx::IntSize(17, 9),
                                                   gfx::ColorSpace2::SRGB);
    ASSERT_TRUE(mProducer);
    EXPECT_TRUE(mGL->fIsEnabled(LOCAL_GL_SCISSOR_TEST));
    realGLboolean mask[4];
    GLfloat clear[4];
    GLint box[4];
    mGL->fGetBooleanv(LOCAL_GL_COLOR_WRITEMASK, mask);
    mGL->fGetFloatv(LOCAL_GL_COLOR_CLEAR_VALUE, clear);
    mGL->fGetIntegerv(LOCAL_GL_SCISSOR_BOX, box);
    EXPECT_EQ(
        (std::array<realGLboolean, 4>{mask[0], mask[1], mask[2], mask[3]}),
        (std::array<realGLboolean, 4>{false, false, false, false}));
    EXPECT_EQ((std::array<GLfloat, 4>{clear[0], clear[1], clear[2], clear[3]}),
              (std::array<GLfloat, 4>{.25f, .5f, .75f, 1.f}));
    EXPECT_EQ((std::array<GLint, 4>{box[0], box[1], box[2], box[3]}),
              (std::array<GLint, 4>{1, 2, 1, 1}));
    if (discard) {
      EXPECT_TRUE(mGL->fIsEnabled(LOCAL_GL_RASTERIZER_DISCARD));
    }
  }
  mGL->fColorMask(true, true, true, true);
  ASSERT_EQ(mProducer->mDesc.type, gl::SharedSurfaceType::EGLSurfaceDMABUF);
  EXPECT_FALSE(mProducer->ToSurfaceDescriptor());
  Maybe<layers::SurfaceDescriptor> previous;
  for (size_t cycle = 0; cycle < 3; ++cycle) {
    ASSERT_TRUE(mProducer->IsValid());
    ASSERT_TRUE(mProducer->BeginWrite());
    EXPECT_FALSE(mProducer->ToSurfaceDescriptor());
    if (previous) {
      OnRenderThread([&] {
        RefPtr<DMABufSurface> stale=DMABufSurface::CreateDMABufSurface(*previous);
        EXPECT_FALSE(stale);
      });
    }
    if (cycle) {
      gl::ScopedBindFramebuffer bind(mGL, mProducer->mFb->mFB);
      mGL->fClearColor(64.f / 255, 32.f / 255, 16.f / 255, 128.f / 255);
      mGL->fClear(LOCAL_GL_COLOR_BUFFER_BIT);
    }
    mProducer->EndWrite();
    if (previous) {
      OnRenderThread([&] {
        RefPtr<DMABufSurface> stale=DMABufSurface::CreateDMABufSurface(*previous);
        EXPECT_FALSE(stale);
      });
    }
    previous = mProducer->ToSurfaceDescriptor();
    ASSERT_TRUE(previous);
    if (cycle) {
      SamplePublication({191, 32, 16, 255}, {64, 32, 16, 255});
    } else {
      SamplePublication({255, 0, 0, 255}, {0, 0, 0, 255});
    }
    ASSERT_FALSE(HasFatalFailure());
    ASSERT_TRUE(mGL->MakeCurrent());
    ASSERT_TRUE(mProducer->BeginRead());
    mProducer->EndRead();
  }
  EXPECT_EQ(mGL->fGetError(), GLenum(LOCAL_GL_NO_ERROR));
}

TEST_F(VulkanForeignImages, OldDeviceMigrationWaitsForComplete) {
  mSwapChain=std::make_unique<gl::SwapChain>();InitSwapChain(*mGL,*mSwapChain,layers::TextureType::DMABUF,true);
  if(!mExpectNative){EXPECT_EQ(mSwapChain->mFactory->GetConsumerType(),layers::TextureType::Unknown);return;}
  ASSERT_EQ(mSwapChain->mFactory->GetConsumerType(),layers::TextureType::DMABUF);
  mProducer=mSwapChain->mFactory->CreateShared(gfx::IntSize(17,9),gfx::ColorSpace2::SRGB);ASSERT_TRUE(mProducer);
  ASSERT_TRUE(mProducer->BeginWrite());
  {gl::ScopedBindFramebuffer bind(mGL,mProducer->mFb->mFB);mGL->fClearColor(64.f/255,32.f/255,16.f/255,128.f/255);mGL->fClear(LOCAL_GL_COLOR_BUFFER_BIT);}
  mProducer->EndWrite();SamplePublication({191,32,16,255},{64,32,16,255},PublicationEnd::Migrate);
}
TEST_F(VulkanForeignImages, AbandonedBorrowNeverReopensProducer) {
  mSwapChain=std::make_unique<gl::SwapChain>();InitSwapChain(*mGL,*mSwapChain,layers::TextureType::DMABUF,true);
  if(!mExpectNative){EXPECT_EQ(mSwapChain->mFactory->GetConsumerType(),layers::TextureType::Unknown);return;}
  ASSERT_EQ(mSwapChain->mFactory->GetConsumerType(),layers::TextureType::DMABUF);
  mProducer=mSwapChain->mFactory->CreateShared(gfx::IntSize(17,9),gfx::ColorSpace2::SRGB);ASSERT_TRUE(mProducer);
  ASSERT_TRUE(mProducer->BeginWrite());
  {gl::ScopedBindFramebuffer bind(mGL,mProducer->mFb->mFB);mGL->fClearColor(64.f/255,32.f/255,16.f/255,128.f/255);mGL->fClear(LOCAL_GL_COLOR_BUFFER_BIT);}
  mProducer->EndWrite();SamplePublication({191,32,16,255},{64,32,16,255},PublicationEnd::Abandon);
}
#endif

}  // namespace mozilla::wr
