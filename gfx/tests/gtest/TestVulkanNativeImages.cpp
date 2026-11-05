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
}  // namespace mozilla::wr
