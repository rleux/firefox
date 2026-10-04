/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <array>
#include <cstring>
#include <functional>
#include <vector>

#include "gfxPlatform.h"
#include "gtest/gtest.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/GPUProcessManager.h"
#include "mozilla/layers/SourceSurfaceSharedData.h"
#include "mozilla/layers/SynchronousTask.h"
#include "mozilla/webrender/RenderBufferTextureHost.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "mozilla/webrender/RenderSharedSurfaceTextureHost.h"
#include "mozilla/webrender/RenderTextureHostWrapper.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/widget/CompositorWidget.h"
#include "nsThreadUtils.h"

namespace mozilla::wr {

using namespace gfx;

namespace {

class RenderExternalBuffer : public ::testing::Test {
 protected:
  void SetUp() override {
    gfxPlatform::GetPlatform();
    gfxPlatform::InitLayersIPC();
    if (!RenderThread::Get()) {
      GTEST_SKIP() << "Requires an in-process render thread";
    }
  }

  template <typename F>
  void OnRenderThread(F&& aTest) {
    layers::SynchronousTask task("RenderExternalBuffer test");
    nsCOMPtr<nsIThread> thread = RenderThread::GetRenderThread();
    ASSERT_EQ(NS_OK,
              thread->Dispatch(NS_NewRunnableFunction(
                                   "RenderExternalBuffer test",
                                   [test = std::move(aTest), &task] {
                                     layers::AutoCompleteTask complete(&task);
                                     test();
                                   }),
                               NS_DISPATCH_NORMAL));
    task.Wait();
  }
};

layers::BufferDescriptor BufferDescriptorForTest() {
  return layers::RGBDescriptor(IntSize(3, 2), SurfaceFormat::B8G8R8A8,
                               ColorSpace2::SRGB, TransferFunction::SRGB);
}

class ContextOnlyTexture final : public RenderTextureHost {
 public:
  WrExternalImage Lock(uint8_t, gl::GLContext*) override {
    mUsedContextPath = true;
    return NativeTextureToWrExternalImage(1, 0, 0, 1, 1);
  }
  void Unlock() override { mUsedContextPath = true; }
  size_t Bytes() override { return 0; }
  bool mUsedContextPath = false;

 private:
  ~ContextOnlyTexture() override = default;
};

class VulkanTexture final : public RenderTextureHost {
 public:
  WrExternalImage LockVulkan(uint8_t aChannel,
                             WrVulkanExternalImages* aImages) override {
    mImages = aImages;
    mChannel = aChannel;
    ++mLocks;
    return NativeTextureToWrExternalImage(19, 0, 0, 2, 2);
  }
  Maybe<VulkanImageRelease> UnlockVulkan(
      WrVulkanExternalImages* aImages) override {
    EXPECT_EQ(aImages, mImages);
    ++mUnlocks;
    if (mReleaseValue) {
      return Some(VulkanImageRelease(nullptr, mReleaseValue.ref()));
    }
    return Nothing();
  }
  void NotifyVulkanRelease(uint64_t aValue,
                           WrVulkanReleaseStatus aStatus) override {
    mNotifications.emplace_back(aValue, aStatus);
    if (mOnRelease) {
      mOnRelease(aValue, aStatus);
    }
  }
  size_t Bytes() override { return 0; }

  WrVulkanExternalImages* mImages = nullptr;
  uint8_t mChannel = 0;
  uint32_t mLocks = 0;
  uint32_t mUnlocks = 0;
  Maybe<uint64_t> mReleaseValue;
  std::vector<std::pair<uint64_t, WrVulkanReleaseStatus>> mNotifications;
  std::function<void(uint64_t, WrVulkanReleaseStatus)> mOnRelease;

 private:
  ~VulkanTexture() override = default;
};

struct TestVulkanRelease {
  uint64_t mValue;
  WrVulkanReleaseStatus* mStatus;
  uint64_t GetValue() const { return mValue; }
  WrVulkanReleaseStatus GetStatus() const { return *mStatus; }
};

WrVulkanConfig DetachedConfig() {
  return {WrWindowHandle::Android(nullptr), {}, {}, false, true, false};
}

}  // namespace

TEST_F(RenderExternalBuffer, BufferMappingAndRelock) {
  OnRenderThread([] {
    std::array<uint8_t, 24> bytes{};
    bytes.fill(0x35);
    RefPtr<RenderTextureHost> host =
        new RenderBufferTextureHost(bytes.data(), BufferDescriptorForTest());
    EXPECT_EQ(host->LockExternalBuffer(1).image_type,
              WrExternalImageType::Invalid);
    host->UnlockExternalBuffer();
    for (uint8_t value : {0x35, 0x9a}) {
      bytes[0] = value;
      auto image = host->LockExternalBuffer(0);
      auto unlock = MakeScopeExit([&] { host->UnlockExternalBuffer(); });
      ASSERT_EQ(image.image_type, WrExternalImageType::RawData);
      ASSERT_EQ(image.buff, bytes.data());
      ASSERT_EQ(image.size, bytes.size());
      EXPECT_EQ(image.buff[0], value);
      EXPECT_EQ(image.buff[23], 0x35);
    }
    host->Destroy();
    EXPECT_EQ(host->LockExternalBuffer(0).image_type,
              WrExternalImageType::Invalid);
    host->UnlockExternalBuffer();
  });
}

TEST_F(RenderExternalBuffer, SharedSurfaceKeepsStorageAndStride) {
  OnRenderThread([] {
    RefPtr<SourceSurfaceSharedData> source = new SourceSurfaceSharedData;
    ASSERT_TRUE(
        source->Init(IntSize(2, 2), 16, SurfaceFormat::B8G8R8A8, false));
    auto* data = source->GetData();
    std::memset(data, 0x72, 32);
    RefPtr<SourceSurfaceSharedDataWrapper> surface =
        new SourceSurfaceSharedDataWrapper;
    surface->Init(source);
    RefPtr<RenderTextureHost> host =
        new RenderSharedSurfaceTextureHost(surface);
    source = nullptr;
    surface = nullptr;
    EXPECT_EQ(host->LockExternalBuffer(1).image_type,
              WrExternalImageType::Invalid);
    for (int i = 0; i < 2; ++i) {
      auto image = host->LockExternalBuffer(0);
      auto unlock = MakeScopeExit([&] { host->UnlockExternalBuffer(); });
      ASSERT_EQ(image.image_type, WrExternalImageType::RawData);
      ASSERT_EQ(image.buff, data);
      ASSERT_EQ(image.size, 32U);
      EXPECT_EQ(image.buff[0], 0x72);
      EXPECT_EQ(image.buff[31], 0x72);
      EXPECT_EQ(host->LockExternalBuffer(0).buff, data);
    }
  });
}

TEST_F(RenderExternalBuffer, ContextTexturesDeclineBufferAccess) {
  OnRenderThread([] {
    RefPtr<ContextOnlyTexture> host = new ContextOnlyTexture;
    EXPECT_EQ(host->LockExternalBuffer(0).image_type,
              WrExternalImageType::Invalid);
    host->UnlockExternalBuffer();
    EXPECT_FALSE(host->mUsedContextPath);
  });
}

TEST_F(RenderExternalBuffer, WrapperRetainsAndForwardsBuffer) {
  ExternalImageId id{uint64_t(GPUProcessManager::Get()->AllocateNamespace())
                     << 32};
  OnRenderThread([id] {
    std::array<uint8_t, 24> bytes{};
    RefPtr<RenderTextureHost> host =
        new RenderBufferTextureHost(bytes.data(), BufferDescriptorForTest());
    bool destroyed = false;
    host->SetDestroyedCallback([&] { destroyed = true; });
    RenderThread::Get()->RegisterExternalImage(id, host.forget());
    RefPtr<RenderTextureHost> wrapper = new RenderTextureHostWrapper(id);
    RenderThread::Get()->UnregisterExternalImage(id);
    EXPECT_FALSE(destroyed);
    {
      auto image = wrapper->LockExternalBuffer(0);
      auto unlock = MakeScopeExit([&] { wrapper->UnlockExternalBuffer(); });
      ASSERT_EQ(image.image_type, WrExternalImageType::RawData);
      ASSERT_EQ(image.buff, bytes.data());
      ASSERT_EQ(image.size, bytes.size());
    }
    wrapper = nullptr;
    EXPECT_TRUE(destroyed);
  });
}

TEST_F(RenderExternalBuffer, VulkanCompositorPreservesBufferFallback) {
  OnRenderThread([] {
    RenderCompositorVulkan compositor(nullptr, DetachedConfig());
    std::array<uint8_t, 24> bytes{};
    RefPtr<RenderTextureHost> host =
        new RenderBufferTextureHost(bytes.data(), BufferDescriptorForTest());
    EXPECT_EQ(compositor.LockExternalImage(host, 1).image_type,
              WrExternalImageType::Invalid);
    compositor.UnlockExternalImage(host);
    auto image = compositor.LockExternalImage(host, 0);
    EXPECT_EQ(image.image_type, WrExternalImageType::RawData);
    EXPECT_EQ(image.buff, bytes.data());
    EXPECT_EQ(image.size, bytes.size());
    compositor.UnlockExternalImage(host);

    RefPtr<ContextOnlyTexture> unsupported = new ContextOnlyTexture;
    EXPECT_EQ(compositor.LockExternalImage(unsupported, 0).image_type,
              WrExternalImageType::Invalid);
    compositor.UnlockExternalImage(unsupported);
    EXPECT_FALSE(unsupported->mUsedContextPath);
  });
}

TEST_F(RenderExternalBuffer, VulkanCompositorDispatchesGpuHooks) {
  OnRenderThread([] {
    RenderCompositorVulkan compositor(nullptr, DetachedConfig());
    RenderCompositor* backend = &compositor;
    RefPtr<VulkanTexture> host = new VulkanTexture;
    auto image = backend->LockExternalImage(host, 3);
    EXPECT_EQ(image.image_type, WrExternalImageType::NativeTexture);
    EXPECT_EQ(host->mImages, nullptr);
    EXPECT_EQ(host->mChannel, 3);
    EXPECT_EQ(host->mLocks, 1U);
    backend->UnlockExternalImage(host);
    EXPECT_EQ(host->mUnlocks, 1U);
  });
}

TEST_F(RenderExternalBuffer, WrapperRetainsAndForwardsVulkanContext) {
  ExternalImageId id{uint64_t(GPUProcessManager::Get()->AllocateNamespace())
                     << 32};
  OnRenderThread([id] {
    RefPtr<VulkanTexture> host = new VulkanTexture;
    RefPtr<RenderTextureHost> registered = host;
    RenderThread::Get()->RegisterExternalImage(id, registered.forget());
    RefPtr<RenderTextureHost> wrapper = new RenderTextureHostWrapper(id);
    RenderThread::Get()->UnregisterExternalImage(id);
    uint8_t token;
    auto* context = reinterpret_cast<WrVulkanExternalImages*>(&token);
    auto image = wrapper->LockVulkan(2, context);
    EXPECT_EQ(image.image_type, WrExternalImageType::NativeTexture);
    EXPECT_EQ(host->mImages, context);
    EXPECT_EQ(host->mChannel, 2);
    host->mReleaseValue = Some(uint64_t(17));
    auto release = wrapper->UnlockVulkan(context);
    ASSERT_TRUE(release);
    EXPECT_EQ(release->GetValue(), 17U);
    wrapper->NotifyVulkanRelease(17, WrVulkanReleaseStatus::Submitted);
    ASSERT_EQ(host->mNotifications.size(), 1U);
    EXPECT_EQ(host->mNotifications[0].first, 17U);
    EXPECT_EQ(host->mNotifications[0].second, WrVulkanReleaseStatus::Submitted);
    EXPECT_EQ(host->mLocks, 1U);
    EXPECT_EQ(host->mUnlocks, 1U);
  });
}

TEST_F(RenderExternalBuffer, VulkanReleasesNotifyOnlyResolvedReceipts) {
  OnRenderThread([] {
    auto status = WrVulkanReleaseStatus::Pending;
    VulkanImageReleaseQueue<TestVulkanRelease> releases;
    RefPtr<VulkanTexture> host = new VulkanTexture;
    EXPECT_FALSE(releases.HasPending());
    releases.Add(host, {7, &status});
    releases.Poll();
    EXPECT_TRUE(releases.HasPending());
    EXPECT_TRUE(host->mNotifications.empty());
    status = WrVulkanReleaseStatus::Submitted;
    releases.Poll();
    releases.Poll();
    EXPECT_FALSE(releases.HasPending());
    ASSERT_EQ(host->mNotifications.size(), 1U);
    EXPECT_EQ(host->mNotifications[0].first, 7U);
    EXPECT_EQ(host->mNotifications[0].second, status);
    status = WrVulkanReleaseStatus::Abandoned;
    releases.Add(host, {8, &status});
    releases.Poll();
    ASSERT_EQ(host->mNotifications.size(), 2U);
    EXPECT_EQ(host->mNotifications[1].first, 8U);
    EXPECT_EQ(host->mNotifications[1].second, status);
  });
}

TEST_F(RenderExternalBuffer, VulkanCancellationPreservesSubmittedReleases) {
  OnRenderThread([] {
    auto submitted = WrVulkanReleaseStatus::Submitted;
    auto pending = WrVulkanReleaseStatus::Pending;
    RefPtr<VulkanTexture> host = new VulkanTexture;
    VulkanImageReleaseQueue<TestVulkanRelease> releases;
    releases.Add(host, {1, &pending});
    releases.Add(host, {2, &submitted});
    releases.Poll(true);
    ASSERT_EQ(host->mNotifications.size(), 2U);
    EXPECT_EQ(host->mNotifications[0].second, WrVulkanReleaseStatus::Abandoned);
    EXPECT_EQ(host->mNotifications[1].second, WrVulkanReleaseStatus::Submitted);
  });
}

TEST_F(RenderExternalBuffer, VulkanQueueRetainsHostUntilTeardownNotification) {
  OnRenderThread([] {
    auto status = WrVulkanReleaseStatus::Pending;
    bool notified = false;
    bool destroyed = false;
    {
      VulkanImageReleaseQueue<TestVulkanRelease> releases;
      RefPtr<VulkanTexture> host = new VulkanTexture;
      host->mOnRelease = [&](uint64_t aValue, WrVulkanReleaseStatus aStatus) {
        EXPECT_FALSE(destroyed);
        EXPECT_EQ(aValue, 41U);
        EXPECT_EQ(aStatus, WrVulkanReleaseStatus::Abandoned);
        notified = true;
      };
      host->SetDestroyedCallback([&] { destroyed = true; });
      releases.Add(host, {41, &status});
      host = nullptr;
      releases.Poll();
      EXPECT_FALSE(notified);
      EXPECT_FALSE(destroyed);
    }
    EXPECT_TRUE(notified);
    EXPECT_TRUE(destroyed);
  });
}

TEST_F(RenderExternalBuffer, VulkanReleaseCallbacksCanEnqueueMoreWork) {
  OnRenderThread([] {
    auto status = WrVulkanReleaseStatus::Submitted;
    VulkanImageReleaseQueue<TestVulkanRelease> releases;
    RefPtr<VulkanTexture> host = new VulkanTexture;
    auto* target = host.get();
    host->mOnRelease = [&](uint64_t aValue, WrVulkanReleaseStatus) {
      if (aValue == 1) {
        releases.Add(target, {2, &status});
      }
    };
    releases.Add(host, {1, &status});
    releases.Poll();
    ASSERT_EQ(host->mNotifications.size(), 1U);
    EXPECT_TRUE(releases.HasPending());
    releases.Poll();
    ASSERT_EQ(host->mNotifications.size(), 2U);
    EXPECT_FALSE(releases.HasPending());
    EXPECT_EQ(host->mNotifications[1].first, 2U);
    host->mNotifications.clear();
    status = WrVulkanReleaseStatus::Pending;
    releases.Add(host, {1, &status});
    releases.Poll(true);
    ASSERT_EQ(host->mNotifications.size(), 2U);
    EXPECT_EQ(host->mNotifications[0].second, WrVulkanReleaseStatus::Abandoned);
    EXPECT_EQ(host->mNotifications[1].second, WrVulkanReleaseStatus::Abandoned);
  });
}

TEST_F(RenderExternalBuffer, VulkanCompositorDefersNotificationsPastUnlock) {
  OnRenderThread([] {
    RenderCompositorVulkan compositor(nullptr, DetachedConfig());
    RefPtr<VulkanTexture> host = new VulkanTexture;
    for (bool success : {true, false}) {
      const auto previous = host->mNotifications.size();
      host->mReleaseValue = Some(uint64_t(previous + 1));
      compositor.UnlockExternalImage(host);
      EXPECT_EQ(host->mNotifications.size(), previous);
      compositor.AfterRender(success);
      ASSERT_EQ(host->mNotifications.size(), previous + 1);
      EXPECT_EQ(host->mNotifications.back().first, previous + 1);
      EXPECT_EQ(host->mNotifications.back().second,
                WrVulkanReleaseStatus::Abandoned);
    }
    host->mReleaseValue = Some(uint64_t(3));
    compositor.UnlockExternalImage(host);
    EXPECT_EQ(host->mNotifications.size(), 2U);
    compositor.SetRenderer(nullptr, WindowId{});
    ASSERT_EQ(host->mNotifications.size(), 3U);
    EXPECT_EQ(host->mNotifications.back().first, 3U);
    EXPECT_EQ(host->mNotifications.back().second,
              WrVulkanReleaseStatus::Abandoned);
  });
}

}  // namespace mozilla::wr
