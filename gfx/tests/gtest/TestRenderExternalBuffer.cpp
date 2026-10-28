/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <algorithm>
#include <array>
#include <chrono>
#include <cstring>
#include <functional>
#include <memory>
#include <thread>
#include <vector>

#include "gfxPlatform.h"
#include "gtest/gtest.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/GPUProcessManager.h"
#include "mozilla/layers/RemoteTextureMap.h"
#include "mozilla/layers/RemoteTextureHostWrapper.h"
#include "mozilla/layers/CompositorThread.h"
#include "mozilla/ipc/Shmem.h"
#include "mozilla/layers/SourceSurfaceSharedData.h"
#include "mozilla/layers/SynchronousTask.h"
#include "mozilla/layers/VulkanImages.h"
#include "mozilla/layers/VulkanTextureHost.h"
#include "mozilla/layers/WebRenderTextureHost.h"
#include "mozilla/webgpu/FrontBufferSnapshot.h"
#include "mozilla/webrender/RenderBufferTextureHost.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "mozilla/webrender/RenderSharedSurfaceTextureHost.h"
#include "mozilla/webrender/RenderTextureHostWrapper.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/webrender/VulkanImageIPC.h"
#include "mozilla/widget/CompositorWidget.h"
#include "nsThreadUtils.h"

#if defined(XP_LINUX) && !defined(ANDROID)
#  include <fcntl.h>
#  include <sys/eventfd.h>
#  include <sys/stat.h>
#  include <sys/syscall.h>
#  include <sys/sysmacros.h>
#  include <unistd.h>

#  include "base/linux_memfd_defs.h"
#  include "mozilla/webrender/RenderDMABUFTextureHost.h"
#  include "mozilla/webrender/RenderVulkanDMABufTextureHost.h"
#  ifdef MOZ_WEBRENDER_VULKAN
#    include "mozilla/AppUnits.h"
#    include "mozilla/layers/AsyncImagePipelineManager.h"
#    include "mozilla/layers/VulkanNativeTextureHost.h"
#    include "mozilla/layers/WebRenderImageHost.h"
#    include "mozilla/webgpu/ffi/wgpu.h"
#    include "mozilla/webrender/RenderVulkanNativeTextureHost.h"
#  endif
#  include "mozilla/widget/DMABufAccess.h"
#  include "mozilla/widget/DMABufDevice.h"
#endif

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

  template <typename F>
  void OnCompositorThread(F&& aTest) {
    layers::SynchronousTask task("Native map compositor test");
    ASSERT_EQ(NS_OK, layers::CompositorThread()->Dispatch(
                         NS_NewRunnableFunction(
                             "Native map compositor test",
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
    status = WrVulkanReleaseStatus::Pending;
    releases.Add(host, {9, &status});
    releases.Poll();
    EXPECT_TRUE(releases.HasPending());
    status = WrVulkanReleaseStatus::Complete;
    releases.Poll();
    EXPECT_FALSE(releases.HasPending());
    ASSERT_EQ(host->mNotifications.size(), 3U);
    EXPECT_EQ(host->mNotifications[2].first, 9U);
    EXPECT_EQ(host->mNotifications[2].second, status);
  });
}

TEST_F(RenderExternalBuffer, VulkanCancellationPreservesSubmittedReleases) {
  OnRenderThread([] {
    auto submitted = WrVulkanReleaseStatus::Submitted;
    auto complete = WrVulkanReleaseStatus::Complete;
    auto pending = WrVulkanReleaseStatus::Pending;
    RefPtr<VulkanTexture> host = new VulkanTexture;
    VulkanImageReleaseQueue<TestVulkanRelease> releases;
    releases.Add(host, {1, &pending});
    releases.Add(host, {2, &submitted});
    releases.Add(host, {3, &complete});
    releases.Poll(true);
    EXPECT_FALSE(releases.HasPending());
    ASSERT_EQ(host->mNotifications.size(), 3U);
    EXPECT_EQ(host->mNotifications[0].second, WrVulkanReleaseStatus::Abandoned);
    EXPECT_EQ(host->mNotifications[1].second, WrVulkanReleaseStatus::Submitted);
    EXPECT_EQ(host->mNotifications[2].second, WrVulkanReleaseStatus::Complete);
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

#if defined(XP_LINUX) && !defined(ANDROID)
namespace {

template <typename T>
bool RoundTripVulkanMessage(const T& aInput, T& aOutput) {
  IPC::Message message(MSG_ROUTING_NONE, 0);
  IPC::MessageWriter writer(message);
  IPC::WriteParam(&writer, aInput);
  IPC::MessageReader reader(message);
  return IPC::ReadParam(&reader, &aOutput);
}

layers::VulkanImagePublication PublicationForTest(int aFd) {
  RefPtr<FileHandleWrapper> memory =
      new FileHandleWrapper(DuplicateFileHandle(aFd));
  RefPtr<FileHandleWrapper> semaphore =
      new FileHandleWrapper(DuplicateFileHandle(aFd));
  VulkanUUID device{}, driver{};
  device.fill(0x12);
  driver.fill(0x34);
  return layers::VulkanImagePublication(
      9, memory, IntSize(3, 2), SurfaceFormat::B8G8R8A8, 0, 4, 16, true, false,
      true, layers::VulkanTimelineDescriptor(semaphore, device, driver, 7));
}

}  // namespace

TEST_F(RenderExternalBuffer, DMABufAccessSharesStateAndLifetime) {
  auto first = widget::DMABufAccess::Create();
  ASSERT_TRUE(first);
  RefPtr<FileHandleWrapper> handle = first->Handle();
  auto second = widget::DMABufAccess::Import(handle);
  ASSERT_TRUE(second);
  EXPECT_TRUE(first->IsUsable());
  ASSERT_TRUE(second->TryLock());
  EXPECT_FALSE(first->TryLock());
  EXPECT_FALSE(first->TryRetire());
  EXPECT_FALSE(first->WaitLock(1));
  second->Unlock();
  ASSERT_TRUE(first->WaitLock(0));
  first->Unlock();
  first = nullptr;
  ASSERT_TRUE(second->TryLock());
  second->Unlock(true);
  EXPECT_FALSE(second->IsUsable());
  second->Unlock();
  EXPECT_FALSE(second->TryLock());
  auto third = widget::DMABufAccess::Import(handle);
  ASSERT_TRUE(third);
  EXPECT_FALSE(third->IsUsable());
}

TEST_F(RenderExternalBuffer, DMABufAccessWakesAndRetires) {
  auto first = widget::DMABufAccess::Create();
  ASSERT_TRUE(first);
  auto second = widget::DMABufAccess::Import(first->Handle());
  ASSERT_TRUE(second);
  ASSERT_TRUE(first->TryLock());
  bool acquired = false;
  std::thread waiter([&] {
    acquired = second->WaitLock(1000);
    if (acquired) {
      second->Unlock();
    }
  });
  first->Unlock();
  waiter.join();
  EXPECT_TRUE(acquired);
  ASSERT_TRUE(first->TryRetire());
  EXPECT_FALSE(second->IsUsable());
  EXPECT_FALSE(second->WaitLock(0));
  first->Unlock();
  EXPECT_FALSE(first->IsUsable());
}

TEST_F(RenderExternalBuffer, DMABufAccessRejectsUnsealedOrWrongSize) {
  EXPECT_FALSE(widget::DMABufAccess::Import(nullptr));
  for (const size_t size : {sizeof(uint32_t), sizeof(uint64_t)}) {
    UniqueFileHandle fd(syscall(SYS_memfd_create, "wr-access-test",
                                MFD_CLOEXEC | MFD_ALLOW_SEALING));
    ASSERT_TRUE(fd);
    ASSERT_EQ(ftruncate(fd.get(), size), 0);
    RefPtr<FileHandleWrapper> handle = new FileHandleWrapper(std::move(fd));
    EXPECT_FALSE(widget::DMABufAccess::Import(handle));
    ASSERT_EQ(fcntl(handle->GetHandle(), F_ADD_SEALS,
                    F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL),
              0);
    auto access = widget::DMABufAccess::Import(handle);
    EXPECT_EQ(bool(access), size == sizeof(uint32_t));
    EXPECT_EQ(ftruncate(handle->GetHandle(), 0), -1);
    if (access) {
      const uint32_t invalid = UINT32_MAX;
      ASSERT_EQ(pwrite(handle->GetHandle(), &invalid, sizeof(invalid), 0),
                ssize_t(sizeof(invalid)));
      EXPECT_FALSE(access->IsUsable());
      EXPECT_FALSE(access->TryLock());
    }
  }
}

TEST_F(RenderExternalBuffer, VulkanPublicationWireValidation) {
  UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
  ASSERT_TRUE(fd);
  const auto original = PublicationForTest(fd.get());
  layers::VulkanImagePublication decoded;
  ASSERT_TRUE(RoundTripVulkanMessage(original, decoded));
  EXPECT_TRUE(ValidateVulkanImagePublication(decoded));
  EXPECT_EQ(decoded.publicationId(), original.publicationId());
  EXPECT_EQ(decoded.size(), original.size());
  EXPECT_EQ(decoded.format(), original.format());
  EXPECT_EQ(decoded.modifier(), original.modifier());
  EXPECT_EQ(decoded.offset(), original.offset());
  EXPECT_EQ(decoded.stride(), original.stride());
  EXPECT_TRUE(decoded.copySrc());
  EXPECT_FALSE(decoded.copyDst());
  EXPECT_TRUE(decoded.colorTarget());
  EXPECT_EQ(decoded.ready().value(), original.ready().value());
  EXPECT_EQ(decoded.ready().deviceUUID(), original.ready().deviceUUID());
  EXPECT_EQ(decoded.ready().driverUUID(), original.ready().driverUUID());
  EXPECT_NE(decoded.memory()->GetHandle(), original.memory()->GetHandle());
  EXPECT_NE(decoded.ready().handle()->GetHandle(),
            original.ready().handle()->GetHandle());
  EXPECT_NE(fcntl(original.memory()->GetHandle(), F_GETFD), -1);
  EXPECT_NE(fcntl(original.ready().handle()->GetHandle(), F_GETFD), -1);
  for (int field = 0; field < 13; ++field) {
    SCOPED_TRACE(field);
    auto invalid = decoded;
    switch (field) {
      case 0:
        invalid.publicationId() = 0;
        break;
      case 1:
        invalid.memory() = nullptr;
        break;
      case 2:
        invalid.ready().handle() = nullptr;
        break;
      case 3:
        invalid.ready().value() = 0;
        break;
      case 4:
        invalid.size().width = 0;
        break;
      case 5:
        invalid.size().height = -1;
        break;
      case 6:
        invalid.format() = SurfaceFormat::A8;
        break;
      case 7:
        invalid.stride() = 8;
        break;
      case 8:
        invalid.stride() = 17;
        break;
      case 9:
        invalid.offset() = 1;
        break;
      case 10:
        invalid.offset() = UINT64_MAX - 3;
        break;
      case 11:
        invalid.stride() = UINT64_MAX - 3;
        break;
      case 12:
        invalid.memory() = new FileHandleWrapper(UniqueFileHandle());
        break;
    }
    EXPECT_FALSE(ValidateVulkanImagePublication(invalid));
  }
}

TEST_F(RenderExternalBuffer, VulkanReturnWireValidation) {
  UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
  ASSERT_TRUE(fd);
  const auto publication = PublicationForTest(fd.get());
  for (auto status :
       {VulkanImageReturnStatus::Unused, VulkanImageReturnStatus::Submitted,
        VulkanImageReturnStatus::Abandoned}) {
    Maybe<layers::VulkanTimelineDescriptor> signal;
    if (status == VulkanImageReturnStatus::Submitted) {
      signal = Some(publication.ready());
      signal->value() = 1;
    }
    layers::VulkanImageReturnMessage original(publication.publicationId(),
                                              status, signal);
    layers::VulkanImageReturnMessage decoded;
    ASSERT_TRUE(RoundTripVulkanMessage(original, decoded));
    EXPECT_TRUE(ValidateVulkanImageReturn(decoded, publication));
    EXPECT_EQ(decoded.status(), status);
    ++decoded.publicationId();
    EXPECT_FALSE(ValidateVulkanImageReturn(decoded, publication));
    decoded = original;
    if (status != VulkanImageReturnStatus::Submitted) {
      decoded.signal() = Some(publication.ready());
      EXPECT_FALSE(ValidateVulkanImageReturn(decoded, publication));
      continue;
    }
    for (int field = 0; field < 5; ++field) {
      SCOPED_TRACE(field);
      decoded = original;
      switch (field) {
        case 0:
          decoded.signal().reset();
          break;
        case 1:
          decoded.signal()->handle() = nullptr;
          break;
        case 2:
          decoded.signal()->value() = 0;
          break;
        case 3:
          ++decoded.signal()->deviceUUID()[0];
          break;
        case 4:
          ++decoded.signal()->driverUUID()[15];
          break;
      }
      EXPECT_FALSE(ValidateVulkanImageReturn(decoded, publication));
    }
  }
  IPC::Message message(MSG_ROUTING_NONE, 0);
  IPC::MessageWriter writer(message);
  IPC::WriteParam(&writer, uint32_t(3));
  IPC::MessageReader reader(message);
  VulkanImageReturnStatus status = VulkanImageReturnStatus::Unused;
  EXPECT_FALSE(IPC::ReadParam(&reader, &status));
  EXPECT_EQ(status, VulkanImageReturnStatus::Unused);
}

TEST_F(RenderExternalBuffer, VulkanWireHostReturnsPublicationOnce) {
  OnRenderThread([] {
    UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
    ASSERT_TRUE(fd);
    auto publication = PublicationForTest(fd.get());
    for (bool fail : {false, true}) {
      size_t returns = 0;
      RefPtr<RenderTextureHost> host = CreateVulkanImageHost(
          publication, [&](layers::VulkanImageReturnMessage&& aReturn) {
            ++returns;
            EXPECT_TRUE(ValidateVulkanImageReturn(aReturn, publication));
            EXPECT_EQ(aReturn.status(), fail
                                            ? VulkanImageReturnStatus::Abandoned
                                            : VulkanImageReturnStatus::Unused);
          });
      ASSERT_TRUE(host);
      EXPECT_EQ(host->GetFormat(), SurfaceFormat::B8G8R8A8);
      EXPECT_EQ(host->Bytes(), 24U);
      if (fail) {
        EXPECT_EQ(host->LockVulkan(0, nullptr).image_type,
                  WrExternalImageType::Invalid);
        auto release = host->UnlockVulkan(nullptr);
        ASSERT_TRUE(release);
        VulkanImageReleaseQueue<> releases;
        releases.Add(host, std::move(release.ref()));
        releases.Poll();
        EXPECT_EQ(returns, 1U);
      }
      host = nullptr;
      EXPECT_EQ(returns, 1U);
    }
    publication.publicationId() = 0;
    RefPtr<RenderTextureHost> rejected = CreateVulkanImageHost(
        publication, [](layers::VulkanImageReturnMessage&&) { ADD_FAILURE(); });
    EXPECT_FALSE(rejected);
  });
}

TEST_F(RenderExternalBuffer, VulkanLayersHostRegistersBeforeRendering) {
  UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
  ASSERT_TRUE(fd);
  auto publication = PublicationForTest(fd.get());
  size_t returns = 0;
  RefPtr<layers::VulkanTextureHost> host = layers::VulkanTextureHost::Create(
      layers::TextureFlags::DEFAULT, publication,
      [&](layers::VulkanImageReturnMessage&& aReturn) {
        EXPECT_TRUE(RenderThread::IsInRenderThread());
        EXPECT_TRUE(ValidateVulkanImageReturn(aReturn, publication));
        EXPECT_EQ(aReturn.status(), VulkanImageReturnStatus::Unused);
        ++returns;
      });
  auto cleanup = MakeScopeExit([&] {
    host = nullptr;
    OnRenderThread([] {});
  });
  ASSERT_TRUE(host);
  EXPECT_EQ(host->GetSize(), publication.size());
  EXPECT_EQ(host->GetFormat(), publication.format());
  EXPECT_EQ(host->NumSubTextures(), 1U);
  RefPtr<DataSourceSurface> snapshot = host->GetAsSurface(nullptr);
  EXPECT_FALSE(snapshot);
  const auto id = host->GetMaybeExternalImageId();
  ASSERT_TRUE(id);
  host->EnsureRenderTexture(Nothing());
  EXPECT_EQ(host->GetMaybeExternalImageId(), id);
  OnRenderThread([&] {
    auto* texture = RenderThread::Get()->GetRenderTexture(id.ref());
    ASSERT_NE(texture, nullptr);
    EXPECT_EQ(texture->GetFormat(), publication.format());
    EXPECT_EQ(texture->Bytes(), 24U);
    EXPECT_EQ(returns, 0U);
  });
  host = nullptr;
  OnRenderThread([&] { EXPECT_EQ(returns, 1U); });
}

TEST_F(RenderExternalBuffer, VulkanRemotePublicationsReturnOnOwnerRemoval) {
  ASSERT_NE(layers::RemoteTextureMap::Get(), nullptr);
  UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
  ASSERT_TRUE(fd);
  auto publication = PublicationForTest(fd.get());
  const auto owner = layers::RemoteTextureOwnerId::GetNext();
  RefPtr<layers::RemoteTextureOwnerClient> client =
      new layers::RemoteTextureOwnerClient(base::GetCurrentProcId());
  size_t returns = 0;
  auto cleanup = MakeScopeExit([&] {
    client->UnregisterAllTextureOwners();
    OnRenderThread([] {});
  });
  const auto rejected = [](layers::VulkanImageReturnMessage&&) {
    ADD_FAILURE();
  };
  auto id = layers::RemoteTextureId::GetNext();
  publication.publicationId() = id.mId;
  EXPECT_FALSE(client->PushVulkanTexture(id, owner, publication, rejected));
  client->RegisterTextureOwner(owner);
  ++publication.publicationId();
  EXPECT_FALSE(client->PushVulkanTexture(id, owner, publication, rejected));
  publication.publicationId() = id.mId;
  auto invalid = publication;
  invalid.memory() = nullptr;
  EXPECT_FALSE(client->PushVulkanTexture(id, owner, invalid, rejected));
  for (size_t i = 0; i < 2; ++i) {
    id = layers::RemoteTextureId::GetNext();
    publication.publicationId() = id.mId;
    ASSERT_TRUE(client->PushVulkanTexture(
        id, owner, publication,
        [&,
         expected = publication](layers::VulkanImageReturnMessage&& aReturn) {
          EXPECT_TRUE(RenderThread::IsInRenderThread());
          EXPECT_TRUE(ValidateVulkanImageReturn(aReturn, expected));
          EXPECT_EQ(aReturn.status(), VulkanImageReturnStatus::Unused);
          ++returns;
        }));
  }
  OnRenderThread([&] { EXPECT_EQ(returns, 0U); });
  client->UnregisterTextureOwner(owner);
  OnRenderThread([&] { EXPECT_EQ(returns, 2U); });
}

TEST_F(RenderExternalBuffer,
       VulkanPublicationReturnCanReenterTextureManagement) {
  for (bool deferred : {false, true}) {
    ExternalImageId id{uint64_t(GPUProcessManager::Get()->AllocateNamespace())
                       << 32};
    bool returned = false;
    OnRenderThread([id, deferred, &returned] {
      UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
      ASSERT_TRUE(fd);
      WrVulkanDmaBufDescriptor image{};
      image.fd = fd.get();
      image.width = image.height = 2;
      image.stride = 8;
      image.format = ImageFormat::RGBA8;
      WrVulkanTimelineDescriptor ready{};
      ready.fd = fd.get();
      RefPtr<RenderTextureHost> host = RenderVulkanDMABufTextureHost::Create(
          image, ready, 1, [&](VulkanImageReturn&& aReturn) {
            EXPECT_EQ(aReturn.mStatus, VulkanImageReturnStatus::Unused);
            (void)RenderThread::Get()->SyncObjectNeeded();
            returned = true;
          });
      ASSERT_TRUE(host);
      RenderThread::Get()->RegisterExternalImage(id, host.forget());
      if (!deferred) {
        RenderThread::Get()->UnregisterExternalImage(id);
        EXPECT_TRUE(returned);
      }
    });
    if (deferred) {
      RenderThread::Get()->UnregisterExternalImage(id);
    }
    OnRenderThread([&] { EXPECT_TRUE(returned); });
  }
}

TEST_F(RenderExternalBuffer,
       VulkanPublicationReturnsUnusedWithoutConsumingFDs) {
  OnRenderThread([] {
    UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
    ASSERT_TRUE(fd);
    WrVulkanDmaBufDescriptor image{};
    image.fd = fd.get();
    image.width = 3;
    image.height = 2;
    image.stride = 16;
    image.format = ImageFormat::BGRA8;
    WrVulkanTimelineDescriptor ready{};
    ready.fd = fd.get();
    Maybe<VulkanImageReturn> returned;
    auto host = RenderVulkanDMABufTextureHost::Create(
        image, ready, 1, [&](VulkanImageReturn&& aReturn) {
          returned.emplace(std::move(aReturn));
        });
    RefPtr<RenderVulkanDMABufTextureHost> retained = std::move(host);
    ASSERT_TRUE(retained);
    EXPECT_NE(fcntl(fd.get(), F_GETFD), -1);
    EXPECT_EQ(retained->Bytes(), 24U);
    EXPECT_EQ(retained->GetFormat(), SurfaceFormat::B8G8R8A8);
    fd.reset();
    EXPECT_FALSE(returned);
    retained = nullptr;
    ASSERT_TRUE(returned);
    EXPECT_EQ(returned->mStatus, VulkanImageReturnStatus::Unused);
    EXPECT_FALSE(returned->mSemaphore);
    EXPECT_EQ(returned->mValue, 0U);
  });
}

TEST_F(RenderExternalBuffer, VulkanPublicationRejectsInvalidMetadata) {
  OnRenderThread([] {
    UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
    ASSERT_TRUE(fd);
    WrVulkanDmaBufDescriptor valid{};
    valid.fd = fd.get();
    valid.width = valid.height = 2;
    valid.stride = 8;
    valid.format = ImageFormat::RGBA8;
    WrVulkanTimelineDescriptor ready{};
    ready.fd = fd.get();
    bool returned = false;
    for (int field = 0; field < 6; ++field) {
      auto image = valid;
      switch (field) {
        case 0:
          image.fd = -1;
          break;
        case 1:
          image.width = 0;
          break;
        case 2:
          image.stride = 0;
          break;
        case 3:
          image.format = ImageFormat::R8;
          break;
        case 4:
          image.device_uuid[0] = 1;
          break;
        case 5:
          image.driver_uuid[0] = 1;
          break;
      }
      RefPtr<RenderVulkanDMABufTextureHost> host =
          RenderVulkanDMABufTextureHost::Create(
              image, ready, 1, [&](VulkanImageReturn&&) { returned = true; });
      EXPECT_FALSE(host);
      EXPECT_FALSE(returned);
      EXPECT_NE(fcntl(fd.get(), F_GETFD), -1);
    }
  });
}

TEST_F(RenderExternalBuffer, VulkanPublicationAbandonsUnavailableContext) {
  OnRenderThread([] {
    UniqueFileHandle fd(open("/dev/null", O_RDONLY | O_CLOEXEC));
    ASSERT_TRUE(fd);
    WrVulkanDmaBufDescriptor image{};
    image.fd = fd.get();
    image.width = image.height = 2;
    image.stride = 8;
    image.format = ImageFormat::RGBA8;
    WrVulkanTimelineDescriptor ready{};
    ready.fd = fd.get();
    Maybe<VulkanImageReturn> returned;
    size_t returns = 0;
    RefPtr<RenderVulkanDMABufTextureHost> host =
        RenderVulkanDMABufTextureHost::Create(
            image, ready, 1, [&](VulkanImageReturn&& aReturn) {
              ++returns;
              returned.emplace(std::move(aReturn));
            });
    ASSERT_TRUE(host);
    EXPECT_EQ(host->LockVulkan(0, nullptr).image_type,
              WrExternalImageType::Invalid);
    EXPECT_EQ(host->LockVulkan(1, nullptr).image_type,
              WrExternalImageType::Invalid);
    EXPECT_FALSE(host->UnlockVulkan(nullptr));
    auto release = host->UnlockVulkan(nullptr);
    ASSERT_TRUE(release);
    EXPECT_FALSE(returned);
    VulkanImageReleaseQueue<> releases;
    releases.Add(host, std::move(release.ref()));
    releases.Poll();
    ASSERT_TRUE(returned);
    EXPECT_EQ(returned->mStatus, VulkanImageReturnStatus::Abandoned);
    EXPECT_EQ(returns, 1U);
    EXPECT_EQ(host->LockVulkan(0, nullptr).image_type,
              WrExternalImageType::Invalid);
    EXPECT_FALSE(host->UnlockVulkan(nullptr));
    host = nullptr;
    EXPECT_EQ(returns, 1U);
  });
}

#  ifdef MOZ_WEBRENDER_VULKAN
struct TestVulkanImage {
  int32_t mMemoryFd;
  int32_t mReadyFd;
  uint64_t mOffset;
  uint64_t mStride;
  uint8_t mDeviceUUID[16];
  uint8_t mDriverUUID[16];
  bool mCopySrc;
};

extern "C" {
void* wr_test_vulkan_image_new(TestVulkanImage*);
Renderer* wr_test_vulkan_image_renderer(void*);
Renderer* wr_test_vulkan_image_next_renderer(void*);
void wr_test_vulkan_image_sample(void*, uint64_t);
void wr_test_vulkan_image_submit_next(void*);
uint64_t wr_test_vulkan_image_value(void*, int32_t, const uint8_t*, const uint8_t*);
void wr_test_vulkan_image_submit(void*);
bool wr_test_vulkan_image_wait(void*, int32_t, const uint8_t*, const uint8_t*,
                               uint64_t);
void wr_test_vulkan_image_delete(void*);
Renderer* wr_test_vulkan_renderer_new();
void wr_test_vulkan_renderer_delete(Renderer*);
void wr_test_vulkan_sync_file_wait();
void wr_test_unified_owner_bootstrap();
void* wr_test_native_canvas_new();
WrVulkanNativePublication* wr_test_native_canvas_prepare(void*, bool, bool,
                                                         bool);
Renderer* wr_test_native_canvas_renderer(void*, bool);
intptr_t wr_test_native_canvas_texture_count(void*, bool);
uint64_t wr_test_native_canvas_sentinel_handle(void*);
void wr_test_native_canvas_check_foreign(void*);
void wr_test_native_canvas_drop_actor(void*, bool);
void wr_test_native_canvas_record(void*, uint64_t, uint64_t);
void wr_test_native_canvas_verify(void*);
void wr_test_native_canvas_finish(void*);
void wr_test_native_canvas_delete(void*);
void* wr_test_native_canvas_authority_new();
void* wr_test_native_canvas_authority_prepare(void*, bool, bool);
void wr_test_native_canvas_authority_release_frame(void*);
void wr_test_native_canvas_authority_drop_actor(void*, bool);
Renderer* wr_test_native_canvas_authority_renderer(void*);
void wr_test_native_canvas_authority_record(void*, uint64_t, uint64_t);
void wr_test_native_canvas_authority_verify(void*);
void wr_test_native_canvas_authority_finish(void*);
void wr_test_native_canvas_authority_delete(void*);
void* wr_test_native_canvas_map_new(WrVulkanNativePublication**,
                                    WrVulkanNativePublication**);
Renderer* wr_test_native_canvas_map_renderer(void*);
void wr_test_native_canvas_map_sample(void*, uint64_t, uint64_t);
void wr_test_native_canvas_map_gate_begin(void*);
bool wr_test_native_canvas_map_gate_release(void*);
intptr_t wr_test_native_canvas_map_texture_count(void*);
bool wr_test_native_canvas_map_live(void*, bool);
void wr_test_native_canvas_map_finish(void*);
void wr_test_native_canvas_map_delete(void*);
void wr_test_native_canvas_routing_state();
void* wr_test_native_canvas_routing_new();
DocumentHandle* wr_test_native_canvas_routing_take_document(void*);
Renderer* wr_test_native_canvas_routing_renderer(void*);
WrVulkanExternalImages* wr_test_native_canvas_routing_images(void*);
int32_t wr_test_native_canvas_routing_max_texture_size(void*);
webgpu::ffi::WGPUNativeCanvasFrame* wr_test_native_canvas_routing_frame(
    void*, uint32_t);
void wr_test_native_canvas_routing_render(void*, uint32_t);
void wr_test_native_canvas_routing_deinit_renderer(void*);
void wr_test_native_canvas_routing_delete(void*);
void* wr_test_foreign_rgb_new(int32_t*);
void wr_test_foreign_rgb_import(void*, int32_t, uint32_t, uint64_t);
void wr_test_foreign_rgb_delete(void*);
int32_t wr_test_foreign_rgb_ready(void*);
}

extern "C" uint64_t wr_test_native_canvas_routing_lock(
    uint64_t aExternalId, uint8_t aChannel, WrVulkanExternalImages* aImages) {
  MOZ_RELEASE_ASSERT(RenderThread::IsInRenderThread() && aImages);
  auto* host =
      RenderThread::Get()->GetRenderTexture(wr::ToExternalImageId(aExternalId));
  MOZ_RELEASE_ASSERT(host);
  const auto image = host->LockVulkan(aChannel, aImages);
  if (image.image_type == WrExternalImageType::Invalid) {
    return 0;
  }
  MOZ_RELEASE_ASSERT(image.image_type == WrExternalImageType::NativeTexture);
  return image.handle;
}

extern "C" void wr_test_native_canvas_routing_unlock(
    uint64_t aExternalId, WrVulkanExternalImages* aImages) {
  MOZ_RELEASE_ASSERT(RenderThread::IsInRenderThread() && aImages);
  auto* host =
      RenderThread::Get()->GetRenderTexture(wr::ToExternalImageId(aExternalId));
  MOZ_RELEASE_ASSERT(host);
  MOZ_RELEASE_ASSERT(!host->UnlockVulkan(aImages));
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanUnifiedOwnerBootstrap) {
  OnRenderThread([] { wr_test_unified_owner_bootstrap(); });
}

static DeviceIntRect NativeCanvasRect(int32_t aX, int32_t aY, int32_t aWidth,
                                      int32_t aHeight) {
  DeviceIntRect rect;
  rect.min.x = aX;
  rect.min.y = aY;
  rect.max.x = aX + aWidth;
  rect.max.y = aY + aHeight;
  return rect;
}

static void CheckNativeCanvasSnapshot(RenderVulkanNativeTextureHost* aHost,
                                      bool aBgra, bool aCopySrc) {
  std::array<uint8_t, 24> expected{17, 51, 102, 255, 100, 60, 20, 128,
                                   7,  31, 53,  64,  137, 11, 83, 192,
                                   23, 67, 101, 255, 11,  19, 31, 64};
  if (aBgra) {
    for (size_t pixel = 0; pixel < 6; ++pixel) {
      std::swap(expected[pixel * 4], expected[pixel * 4 + 2]);
    }
  }
  std::array<uint8_t, 40> output;
  output.fill(0xa7);
  auto format = ImageFormat::R8;
  for (const auto& invalid :
       {NativeCanvasRect(-1, 0, 3, 2), NativeCanvasRect(0, 0, 0, 2),
        NativeCanvasRect(2, 0, 2, 2)}) {
    EXPECT_FALSE(
        aHost->Snapshot(invalid, 16, output.data(), output.size(), &format));
    EXPECT_TRUE(std::all_of(output.begin(), output.end(),
                            [](uint8_t v) { return v == 0xa7; }));
    EXPECT_EQ(format, ImageFormat::R8);
  }
  const auto full = NativeCanvasRect(0, 0, 3, 2);
  EXPECT_FALSE(
      aHost->Snapshot(full, 11, output.data(), output.size(), &format));
  EXPECT_FALSE(aHost->Snapshot(full, 16, output.data(), 31, &format));
  EXPECT_FALSE(
      aHost->Snapshot(full, SIZE_MAX, output.data(), output.size(), &format));
  EXPECT_FALSE(aHost->Snapshot(full, 16, nullptr, output.size(), &format));
  EXPECT_FALSE(
      aHost->Snapshot(full, 16, output.data(), output.size(), nullptr));
  EXPECT_TRUE(std::all_of(output.begin(), output.end(),
                          [](uint8_t v) { return v == 0xa7; }));
  EXPECT_EQ(format, ImageFormat::R8);
  bool copied = false;
  bool offRenderThread = false;
  std::thread canvas([&] {
    offRenderThread = !RenderThread::IsInRenderThread();
    copied = aHost->Snapshot(full, 16, output.data(), output.size(), &format);
  });
  canvas.join();
  ASSERT_TRUE(offRenderThread);
  ASSERT_EQ(copied, aCopySrc);
  if (!aCopySrc) {
    EXPECT_TRUE(std::all_of(output.begin(), output.end(),
                            [](uint8_t v) { return v == 0xa7; }));
    EXPECT_EQ(format, ImageFormat::R8);
    return;
  }
  EXPECT_EQ(format, aBgra ? ImageFormat::BGRA8 : ImageFormat::RGBA8);
  for (size_t row = 0; row < 2; ++row) {
    EXPECT_TRUE(std::equal(expected.begin() + row * 12,
                           expected.begin() + (row + 1) * 12,
                           output.begin() + row * 16));
    for (size_t pad = 12; pad < 16; ++pad) {
      EXPECT_EQ(output[row * 16 + pad], 0);
    }
  }
  for (size_t byte = 32; byte < output.size(); ++byte) {
    EXPECT_EQ(output[byte], 0xa7);
  }
  output.fill(0xa7);
  ASSERT_TRUE(aHost->Snapshot(NativeCanvasRect(1, 1, 2, 1), 12, output.data(),
                              output.size(), &format));
  EXPECT_TRUE(
      std::equal(expected.begin() + 16, expected.end(), output.begin()));
  for (size_t byte = 8; byte < 12; ++byte) {
    EXPECT_EQ(output[byte], 0);
  }
  for (size_t byte = 12; byte < output.size(); ++byte) {
    EXPECT_EQ(output[byte], 0xa7);
  }
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanNativeCanvasHost) {
  OnRenderThread([] {
    auto* fixture = wr_test_native_canvas_new();
    ASSERT_TRUE(fixture);
    auto deleteFixture =
        MakeScopeExit([&] { wr_test_native_canvas_delete(fixture); });
    for (int caseIndex = 0; caseIndex < 5; ++caseIndex) {
      const bool bgra = caseIndex == 2 || caseIndex == 4;
      const bool srgb = caseIndex == 1 || caseIndex == 4;
      const bool copySrc = caseIndex != 3;
      auto* publication =
          wr_test_native_canvas_prepare(fixture, bgra, srgb, copySrc);
      ASSERT_TRUE(publication);
      RefPtr<RenderVulkanNativeTextureHost> host =
          RenderVulkanNativeTextureHost::Create(publication);
      ASSERT_TRUE(host);
      auto* images = wr_vulkan_external_images_new(
          wr_test_native_canvas_renderer(fixture, false));
      auto* alias = wr_vulkan_external_images_new(
          wr_test_native_canvas_renderer(fixture, false));
      auto* foreign = wr_vulkan_external_images_new(
          wr_test_native_canvas_renderer(fixture, true));
      auto cleanup = MakeScopeExit([&] {
        host = nullptr;
        if (images) wr_vulkan_external_images_delete(images);
        if (alias) wr_vulkan_external_images_delete(alias);
        if (foreign) wr_vulkan_external_images_delete(foreign);
      });
      ASSERT_TRUE(images && alias && foreign);
      EXPECT_EQ(host->GetSize(), IntSize(3, 2));
      EXPECT_EQ(host->Bytes(), 24U);
      EXPECT_EQ(host->GetFormat(),
                bgra ? SurfaceFormat::B8G8R8A8 : SurfaceFormat::R8G8B8A8);
      const auto normal = host->LockVulkan(0, images);
      const auto opaque = host->LockVulkan(1, images);
      ASSERT_EQ(normal.image_type, WrExternalImageType::NativeTexture);
      ASSERT_EQ(opaque.image_type, WrExternalImageType::NativeTexture);
      EXPECT_NE(normal.handle, opaque.handle);
      EXPECT_EQ(host->LockVulkan(0, alias).handle, normal.handle);
      EXPECT_EQ(host->LockVulkan(2, images).image_type,
                WrExternalImageType::Invalid);
      EXPECT_EQ(host->LockVulkan(0, nullptr).image_type,
                WrExternalImageType::Invalid);
      if (caseIndex == 0) {
        EXPECT_EQ(wr_test_native_canvas_sentinel_handle(fixture),
                  normal.handle);
        const auto before = wr_test_native_canvas_texture_count(fixture, true);
        EXPECT_EQ(host->LockVulkan(0, foreign).image_type,
                  WrExternalImageType::Invalid);
        EXPECT_EQ(wr_test_native_canvas_texture_count(fixture, true), before);
        EXPECT_EQ(host->LockVulkan(0, images).handle, normal.handle);
        wr_test_native_canvas_check_foreign(fixture);
      }
      wr_test_native_canvas_drop_actor(fixture, caseIndex == 4);
      const auto beforeSnapshot =
          wr_test_native_canvas_texture_count(fixture, false);
      CheckNativeCanvasSnapshot(host, bgra, copySrc);
      EXPECT_EQ(wr_test_native_canvas_texture_count(fixture, false),
                beforeSnapshot);
      wr_test_native_canvas_record(fixture, normal.handle, opaque.handle);
      EXPECT_FALSE(host->UnlockVulkan(images));
      host = nullptr;
      wr_test_native_canvas_verify(fixture);
      wr_vulkan_external_images_delete(images);
      wr_vulkan_external_images_delete(alias);
      wr_vulkan_external_images_delete(foreign);
      images = alias = foreign = nullptr;
      wr_test_native_canvas_finish(fixture);
    }
  });
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanNativeCanvasAuthority) {
  OnRenderThread([] {
    auto* fixture = wr_test_native_canvas_authority_new();
    ASSERT_TRUE(fixture);
    auto deleteFixture =
        MakeScopeExit([&] { wr_test_native_canvas_authority_delete(fixture); });
    EXPECT_FALSE(wr_vulkan_native_publication_from_webgpu_frame(nullptr));
    for (int caseIndex = 0; caseIndex < 4; ++caseIndex) {
      const bool bgra = caseIndex % 2 != 0;
      auto* frame =
          wr_test_native_canvas_authority_prepare(fixture, bgra, caseIndex < 2);
      auto releaseFrame = MakeScopeExit([&] {
        if (frame) wr_test_native_canvas_authority_release_frame(frame);
      });
      ASSERT_TRUE(frame);
      auto* publication = wr_vulkan_native_publication_from_webgpu_frame(frame);
      ASSERT_TRUE(publication);
      RefPtr<RenderVulkanNativeTextureHost> host =
          RenderVulkanNativeTextureHost::Create(publication);
      ASSERT_TRUE(host);
      auto* second = wr_vulkan_native_publication_from_webgpu_frame(frame);
      ASSERT_TRUE(second);
      wr_vulkan_native_publication_delete(second);
      wr_test_native_canvas_authority_release_frame(frame);
      frame = nullptr;
      wr_test_native_canvas_authority_drop_actor(fixture, caseIndex == 3);
      CheckNativeCanvasSnapshot(host, bgra, true);
      auto* images = wr_vulkan_external_images_new(
          wr_test_native_canvas_authority_renderer(fixture));
      auto cleanup = MakeScopeExit([&] {
        host = nullptr;
        if (images) wr_vulkan_external_images_delete(images);
      });
      ASSERT_TRUE(images);
      const auto normal = host->LockVulkan(0, images);
      const auto opaque = host->LockVulkan(1, images);
      ASSERT_EQ(normal.image_type, WrExternalImageType::NativeTexture);
      ASSERT_EQ(opaque.image_type, WrExternalImageType::NativeTexture);
      EXPECT_NE(normal.handle, opaque.handle);
      wr_test_native_canvas_authority_record(fixture, normal.handle,
                                             opaque.handle);
      EXPECT_FALSE(host->UnlockVulkan(images));
      host = nullptr;
      wr_test_native_canvas_authority_verify(fixture);
      wr_vulkan_external_images_delete(images);
      images = nullptr;
      wr_test_native_canvas_authority_finish(fixture);
    }
  });
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanNativeCanvasMap) {
  auto* map = layers::RemoteTextureMap::Get();
  ASSERT_TRUE(map);
  const auto owner = layers::RemoteTextureOwnerId::GetNext();
  const auto firstId = layers::RemoteTextureId::GetNext();
  const auto latestId = layers::RemoteTextureId::GetNext();
  const auto pid = base::GetCurrentProcId();
  RefPtr<layers::RemoteTextureOwnerClient> client =
      new layers::RemoteTextureOwnerClient(pid);
  RefPtr<layers::TextureHost> wrapper;
  layers::CompositableTextureHostRef compositable;
  Maybe<ExternalImageId> external;
  WrVulkanNativePublication* first = nullptr;
  WrVulkanNativePublication* latest = nullptr;
  WrVulkanExternalImages* images = nullptr;
  void* fixture = nullptr;
  bool registered = false;
  bool gated = false;
  std::thread snapshotWorker;
  auto cleanup = MakeScopeExit([&] {
    if (gated) {
      OnRenderThread([&] {
        EXPECT_TRUE(wr_test_native_canvas_map_gate_release(fixture));
      });
      gated = false;
    }
    if (snapshotWorker.joinable()) snapshotWorker.join();
    if (registered) client->UnregisterTextureOwner(owner);
    if (wrapper) {
      OnCompositorThread([&] {
        compositable = nullptr;
        wrapper = nullptr;
      });
    }
    OnRenderThread([&] {
      if (images) wr_vulkan_external_images_delete(images);
      if (first) wr_vulkan_native_publication_delete(first);
      if (latest) wr_vulkan_native_publication_delete(latest);
      if (fixture) wr_test_native_canvas_map_delete(fixture);
    });
  });
  OnRenderThread(
      [&] { fixture = wr_test_native_canvas_map_new(&first, &latest); });
  ASSERT_TRUE(fixture && first && latest);
  EXPECT_FALSE(client->PushNativeTexture(
      firstId, owner, wr_vulkan_native_publication_clone(first)));
  client->RegisterTextureOwner(owner);
  registered = true;
  const bool pushedFirst = client->PushNativeTexture(firstId, owner, first);
  first = nullptr;
  ASSERT_TRUE(pushedFirst);
  OnCompositorThread([&] {
    wrapper = map->GetOrCreateRemoteTextureHostWrapper(
        firstId, owner, pid, IntSize(3, 2), layers::TextureFlags::DEFAULT);
    ASSERT_TRUE(wrapper);
    map->GetRemoteTexture(wrapper->AsRemoteTextureHostWrapper());
    ASSERT_TRUE(wrapper->IsValid());
    compositable = wrapper.get();
    EXPECT_EQ(wrapper->GetFormat(), SurfaceFormat::R8G8B8A8);
    auto* webRender = wrapper->AsWebRenderTextureHost();
    ASSERT_TRUE(webRender);
    external = Some(webRender->GetExternalImageKey());
  });
  ASSERT_TRUE(wrapper);
  ASSERT_TRUE(external);
  std::unique_ptr<WrVulkanNativePublication,
                  decltype(&wr_vulkan_native_publication_delete)>
      snapshotBounds(wr_vulkan_native_publication_clone(latest),
                     &wr_vulkan_native_publication_delete);
  ASSERT_TRUE(snapshotBounds);
  const bool pushedLatest = client->PushNativeTexture(latestId, owner, latest);
  latest = nullptr;
  ASSERT_TRUE(pushedLatest);
  uint64_t normal = 0;
  uint64_t opaque = 0;
  OnRenderThread([&] {
    images = wr_vulkan_external_images_new(
        wr_test_native_canvas_map_renderer(fixture));
    ASSERT_TRUE(images);
    auto* renderHost = RenderThread::Get()->GetRenderTexture(external.ref());
    ASSERT_TRUE(renderHost);
    const auto a = renderHost->LockVulkan(0, images);
    const auto b = renderHost->LockVulkan(1, images);
    ASSERT_EQ(a.image_type, WrExternalImageType::NativeTexture);
    ASSERT_EQ(b.image_type, WrExternalImageType::NativeTexture);
    normal = a.handle;
    opaque = b.handle;
    wr_test_native_canvas_map_sample(fixture, normal, opaque);
    EXPECT_FALSE(renderHost->UnlockVulkan(images));
  });
  ASSERT_NE(normal, 0U);
  ASSERT_NE(opaque, 0U);
  std::array<uint8_t, 40> destination;
  std::array<uint8_t, 31> shortDestination;
  destination.fill(0xa7);
  shortDestination.fill(0xa7);
  const auto invalidSnapshot = [&](uint8_t* bytes, size_t length,
                                   size_t stride) {
    DeviceIntRect rect;
    rect.min.x = 0;
    rect.min.y = 0;
    rect.max.x = 3;
    rect.max.y = 2;
    auto format = ImageFormat::R8;
    return wr_vulkan_native_publication_snapshot(snapshotBounds.get(), rect,
                                                 stride, bytes, length, &format);
  };
  EXPECT_FALSE(invalidSnapshot(nullptr, 0, 16));
  EXPECT_FALSE(invalidSnapshot(destination.data(), destination.size(), 11));
  EXPECT_FALSE(invalidSnapshot(shortDestination.data(), shortDestination.size(),
                               16));
  EXPECT_FALSE(invalidSnapshot(destination.data(), destination.size(), SIZE_MAX));
  EXPECT_TRUE(std::all_of(destination.begin(), destination.end(),
                          [](uint8_t value) { return value == 0xa7; }));
  EXPECT_TRUE(std::all_of(shortDestination.begin(), shortDestination.end(),
                          [](uint8_t value) { return value == 0xa7; }));
  snapshotBounds.reset();
  intptr_t before = 0;
  OnRenderThread([&] {
    wr_test_native_canvas_map_gate_begin(fixture);
    before = wr_test_native_canvas_map_texture_count(fixture);
  });
  gated = true;
  webgpu::FrontBufferSnapshot snapshot;
  bool offRenderThread = false;
  snapshotWorker = std::thread([&] {
    offRenderThread = !RenderThread::IsInRenderThread();
    snapshot = map->GetLatestBufferSnapshot(owner, pid);
  });
  bool admitted = false;
  const auto deadline =
      std::chrono::steady_clock::now() + std::chrono::seconds(2);
  while (std::chrono::steady_clock::now() < deadline) {
    OnRenderThread([&] {
      admitted = wr_test_native_canvas_map_texture_count(fixture) > before;
    });
    if (admitted) break;
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  }
  bool retained = false;
  if (admitted) {
    client->UnregisterTextureOwner(owner);
    registered = false;
    OnRenderThread([&] {
      retained = wr_test_native_canvas_map_live(fixture, true) &&
                 wr_test_native_canvas_map_texture_count(fixture) > before;
    });
  }
  bool explicitRelease = false;
  OnRenderThread([&] {
    explicitRelease = wr_test_native_canvas_map_gate_release(fixture);
  });
  gated = false;
  snapshotWorker.join();
  if (registered) {
    client->UnregisterTextureOwner(owner);
    registered = false;
  }
  ASSERT_TRUE(explicitRelease);
  ASSERT_TRUE(admitted);
  ASSERT_TRUE(retained);
  ASSERT_TRUE(offRenderThread);
  ASSERT_TRUE(snapshot.handle);
  ASSERT_EQ(snapshot.width, 3U);
  ASSERT_EQ(snapshot.height, 2U);
  ASSERT_EQ(snapshot.byteStride, 12U);
  auto snapshotMapping = snapshot.handle.Map();
  ASSERT_TRUE(snapshotMapping);
  ASSERT_GE(snapshotMapping.Size(), snapshot.byteStride * snapshot.height);
  const std::array<uint8_t, 24> expected{11, 37, 201, 255, 13, 99,  5,  128,
                                         31, 7,  53,  64,  11, 137, 83, 192,
                                         67, 23, 101, 255, 19, 11,  31, 64};
  for (size_t row = 0; row < 2; ++row) {
    EXPECT_TRUE(std::equal(expected.begin() + row * 12,
                           expected.begin() + (row + 1) * 12,
                           snapshotMapping.DataAs<uint8_t>() + row * snapshot.byteStride));
  }
  OnCompositorThread([] {});
  OnRenderThread([&] {
    EXPECT_FALSE(wr_test_native_canvas_map_live(fixture, true));
    EXPECT_TRUE(wr_test_native_canvas_map_live(fixture, false));
    auto* renderHost = RenderThread::Get()->GetRenderTexture(external.ref());
    ASSERT_TRUE(renderHost);
    EXPECT_EQ(renderHost->LockVulkan(0, images).handle, normal);
    wr_test_native_canvas_map_sample(fixture, normal, opaque);
    EXPECT_FALSE(renderHost->UnlockVulkan(images));
  });
  OnCompositorThread([&] {
    compositable = nullptr;
    wrapper = nullptr;
  });
  OnRenderThread([&] {
    wr_vulkan_external_images_delete(images);
    images = nullptr;
    wr_test_native_canvas_map_finish(fixture);
  });
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanNativeCanvasRouting) {
  void* fixture = nullptr;
  RefPtr<WebRenderAPI> api;
  RefPtr<layers::AsyncImagePipelineManager> manager;
  RefPtr<layers::WebRenderImageHost> imageHost;
  RefPtr<layers::RemoteTextureOwnerClient> client;
  Maybe<PipelineId> active;
  auto cleanup = MakeScopeExit([&] {
    OnCompositorThread([&] {
      if (client) client->UnregisterAllTextureOwners();
      if (manager) {
        if (active) {
          TransactionBuilder transaction(api);
          manager->RemoveAsyncImagePipeline(active.ref(), nullptr, transaction);
          api->SendTransaction(transaction);
          active = Nothing();
        }
        manager->Destroy();
        manager = nullptr;
      }
      if (imageHost) imageHost->OnReleased();
      imageHost = nullptr;
      client = nullptr;
    });
    OnRenderThread([&] {
      if (api) {
        api->DestroyRendererForTesting(
            wr_test_native_canvas_routing_deinit_renderer, fixture);
      } else if (fixture) {
        wr_test_native_canvas_routing_deinit_renderer(fixture);
      }
    });
    api = nullptr;
    OnRenderThread([&] {
      if (fixture) wr_test_native_canvas_routing_delete(fixture);
    });
  });
  OnRenderThread([&] {
    wr_test_native_canvas_routing_state();
    fixture = wr_test_native_canvas_routing_new();
    ASSERT_TRUE(fixture);
    WebRenderCapabilities capabilities;
    capabilities.mMaxTextureSize =
        wr_test_native_canvas_routing_max_texture_size(fixture);
    ASSERT_GE(capabilities.mMaxTextureSize, 3);
    auto* document = wr_test_native_canvas_routing_take_document(fixture);
    ASSERT_TRUE(document);
    api = WebRenderAPI::CreateForTesting(document, capabilities);
  });
  ASSERT_TRUE(api);
  const auto pid = base::GetCurrentProcId();
  OnCompositorThread([&] {
    RefPtr<WebRenderAPI> managerApi = api;
    manager = MakeRefPtr<layers::AsyncImagePipelineManager>(
        std::move(managerApi), false);
    imageHost = MakeRefPtr<layers::WebRenderImageHost>(layers::TextureInfo(
        layers::CompositableType::IMAGE, layers::ImageUsageType::Canvas,
        layers::TextureFlags::DEFAULT));
    client = MakeRefPtr<layers::RemoteTextureOwnerClient>(pid);
  });
  ASSERT_TRUE(manager && imageHost && client);
  for (uint32_t alpha = 0; alpha < 2; ++alpha) {
    const bool opaque = alpha == 0;
    const PipelineId pipeline{0, 71 + alpha};
    const PipelineId root{0, 70};
    const auto owner = layers::RemoteTextureOwnerId::GetNext();
    bool registered = false;
    bool blankApplied = false;
    OnCompositorThread([&] {
      registered = client->RegisterTextureOwner(owner);
      ASSERT_TRUE(registered);
      manager->AddAsyncImagePipeline(pipeline, imageHost);
      active = Some(pipeline);
      manager->UpdateAsyncImagePipeline(
          pipeline, LayoutDeviceRect(0, 0, 3, 2), WrRotation::Degree0,
          ImageRendering::Pixelated, MixBlendMode::Normal);
      RefPtr<layers::RemoteTextureOwnerClient> foreign =
          MakeRefPtr<layers::RemoteTextureOwnerClient>(pid);
      EXPECT_FALSE(foreign->RegisterTextureOwner(owner));
      EXPECT_FALSE(foreign->IsRegistered(owner));
      EXPECT_TRUE(client->IsRegistered(owner));
      const auto blank = layers::RemoteTextureId::GetNext();
      ASSERT_TRUE(client->PushNativeErrorTexture(
          blank, owner, IntSize(3, 2), SurfaceFormat::R8G8B8A8, opaque));
      imageHost->PushPendingRemoteTexture(blank, owner, pid, IntSize(3, 2),
                                          layers::TextureFlags::DEFAULT);
      imageHost->UseRemoteTexture(false);
      TransactionBuilder transaction(api);
      manager->ApplyAsyncImageForPipeline(pipeline, transaction, transaction,
                                          nullptr, nullptr);
      auto* native =
          imageHost->GetCurrentTextureHost()->AsVulkanNativeTextureHost();
      ASSERT_TRUE(native && native->IsError());
      EXPECT_EQ(native->NumSubTextures(), 0U);
      EXPECT_FALSE(native->GetMaybeExternalImageId());
      DisplayListBuilder builder(root, api->GetCapabilities().mBackendType);
      builder.Begin(AppUnitsPerCSSPixel());
      const auto bounds = wr::ToLayoutRect(LayoutDeviceRect(0, 0, 3, 2));
      builder.PushRect(
          bounds, bounds, true, false, false,
          ColorF{23.0f / 255.0f, 47.0f / 255.0f, 89.0f / 255.0f, 1.0f});
      builder.PushIFrame(LayoutDeviceRect(0, 0, 3, 2), true, pipeline, false);
      BuiltDisplayList display;
      builder.End(display);
      transaction.SetRootPipeline(root);
      transaction.SetDocumentView(LayoutDeviceIntRect(0, 0, 3, 2));
      transaction.SetDisplayList(
          Epoch{alpha + 1}, api->GetNamespace(), root, display.dl_desc,
          display.dl_items, display.dl_spatial_tree, display.dl_interner_delta);

      transaction.GenerateFrame(VsyncId(), false, false,
                                RenderReasons::TESTING);
      api->SendTransaction(transaction);
      blankApplied = true;
    });
    ASSERT_TRUE(registered && blankApplied);
    OnRenderThread(
        [&] { wr_test_native_canvas_routing_render(fixture, alpha * 3 + 1); });
    for (uint32_t step = 0; step < 3; ++step) {
      const auto id = layers::RemoteTextureId::GetNext();
      const bool error = step == 1;
      webgpu::ffi::WGPUNativeCanvasFrame* frame = nullptr;
      if (!error) {
        OnRenderThread([&] {
          frame = wr_test_native_canvas_routing_frame(
              fixture, alpha * 2 + (step == 2 ? 1 : 0));
        });
        ASSERT_TRUE(frame);
      }
      auto releaseFrame = MakeScopeExit([&] {
        if (frame) webgpu::ffi::wgpu_native_canvas_frame_delete(frame);
      });
      bool applied = false;
      Maybe<ExternalImageId> errorWrapper;
      OnCompositorThread([&] {
        bool accepted;
        if (error) {
          accepted = client->PushNativeErrorTexture(
              id, owner, IntSize(3, 2), SurfaceFormat::R8G8B8A8, opaque);
        } else {
          auto* publication =
              wr_vulkan_native_publication_from_webgpu_frame(frame);
          ASSERT_TRUE(publication);
          accepted = client->PushNativeTexture(id, owner, publication, opaque);
        }
        ASSERT_TRUE(accepted);
        imageHost->PushPendingRemoteTexture(id, owner, pid, IntSize(3, 2),
                                            layers::TextureFlags::DEFAULT);
        imageHost->UseRemoteTexture(false);
        auto* texture = imageHost->GetCurrentTextureHost();
        ASSERT_TRUE(texture);
        layers::RemoteTextureMap::Get()->GetRemoteTexture(
            texture->AsRemoteTextureHostWrapper());
        auto* native = texture->AsVulkanNativeTextureHost();
        ASSERT_TRUE(native);
        EXPECT_EQ(native->IsError(), error);
        EXPECT_EQ(texture->NumSubTextures(), error ? 0U : 1U);
        layers::TextureFlags flags =
            (native->GetFlags() & ~layers::TextureFlags::IS_OPAQUE) |
            layers::TextureFlags::NON_PREMULTIPLIED;
        if (!opaque) flags |= layers::TextureFlags::IS_OPAQUE;
        native->SetFlags(flags);
        layers::TextureFlags wrapperFlags =
            (texture->GetFlags() & ~layers::TextureFlags::IS_OPAQUE) |
            layers::TextureFlags::NON_PREMULTIPLIED;
        if (!opaque) wrapperFlags |= layers::TextureFlags::IS_OPAQUE;
        texture->SetFlags(wrapperFlags);
        if (error) {
          EXPECT_FALSE(native->GetMaybeExternalImageId());
          errorWrapper = texture->GetMaybeExternalImageId();
          ASSERT_TRUE(errorWrapper);
        }
        if (step == 0) {
          EXPECT_FALSE(client->PushNativeErrorTexture(
              id, owner, IntSize(3, 2), SurfaceFormat::R8G8B8A8, opaque));
          const auto otherOwner = layers::RemoteTextureOwnerId::GetNext();
          ASSERT_TRUE(client->RegisterTextureOwner(otherOwner));
          EXPECT_FALSE(client->PushNativeErrorTexture(
              id, otherOwner, IntSize(3, 2), SurfaceFormat::R8G8B8A8, opaque));
          EXPECT_FALSE(layers::RemoteTextureMap::Get()
                           ->GetOrCreateRemoteTextureHostWrapper(
                               id, otherOwner, pid, IntSize(3, 2),
                               layers::TextureFlags::DEFAULT));
          client->UnregisterTextureOwner(otherOwner);
          auto rightful = layers::RemoteTextureMap::Get()
                              ->GetOrCreateRemoteTextureHostWrapper(
                                  id, owner, pid, IntSize(3, 2),
                                  layers::TextureFlags::DEFAULT);
          EXPECT_EQ(rightful.get(), texture);
        }
        TransactionBuilder transaction(api);
        manager->ApplyAsyncImageForPipeline(pipeline, transaction, transaction,
                                            nullptr, nullptr);
        EXPECT_FALSE(transaction.IsResourceUpdatesEmpty());

        transaction.GenerateFrame(VsyncId(), false, false,
                                  RenderReasons::TESTING);
        api->SendTransaction(transaction);
        applied = true;
      });
      ASSERT_TRUE(applied);
      if (frame) {
        webgpu::ffi::wgpu_native_canvas_frame_delete(frame);
        frame = nullptr;
      }
      OnRenderThread([&] {
        if (errorWrapper) {
          EXPECT_FALSE(
              RenderThread::Get()->GetRenderTexture(errorWrapper.ref()));
        }
        wr_test_native_canvas_routing_render(fixture, alpha * 3 + step);
      });
      if (step == 0) {
        OnCompositorThread([&] {
          TransactionBuilder transaction(api);
          manager->ApplyAsyncImageForPipeline(
              pipeline, static_cast<layers::TextureHost*>(nullptr),
              transaction);
          EXPECT_TRUE(transaction.IsResourceUpdatesEmpty());
          transaction.GenerateFrame(VsyncId(), false, false,
                                    RenderReasons::TESTING);
          api->SendTransaction(transaction);
        });
        OnRenderThread(
            [&] { wr_test_native_canvas_routing_render(fixture, alpha * 3); });
      }
    }
    OnCompositorThread([&] {
      imageHost->ClearImages(layers::ClearImagesType::All);
      EXPECT_FALSE(imageHost->GetCurrentTextureHost());
      TransactionBuilder transaction(api);
      manager->ApplyAsyncImageForPipeline(pipeline, transaction, transaction,
                                          nullptr, nullptr);
      EXPECT_FALSE(transaction.IsResourceUpdatesEmpty());
      transaction.GenerateFrame(VsyncId(), false, false,
                                RenderReasons::TESTING);
      api->SendTransaction(transaction);
    });
    OnRenderThread([&] { wr_test_native_canvas_routing_render(fixture, 4); });
    OnCompositorThread([&] {
      TransactionBuilder transaction(api);
      manager->RemoveAsyncImagePipeline(pipeline, nullptr, transaction);
      api->SendTransaction(transaction);
      active = Nothing();
      imageHost->OnReleased();
      client->UnregisterTextureOwner(owner);
    });
  }
}

static layers::SurfaceDescriptorDMABuf ForeignPublicationForTest(
    int aMemory, int aReady, uint32_t aFormat, uint32_t aStride,
    widget::DMABufAccess& aAccess) {
  layers::SurfaceDescriptorDMABuf desc;
  desc.bufferType() = DMABufSurface::SURFACE_RGBA;
  desc.fourccFormat() = aFormat;
  desc.modifier().AppendElement(0);
  desc.fds().AppendElement(
      WrapNotNull(MakeRefPtr<FileHandleWrapper>(DuplicateFileHandle(aMemory))));
  desc.width().AppendElement(17);
  desc.height().AppendElement(9);
  desc.strides().AppendElement(aStride);
  desc.offsets().AppendElement(0);
  desc.fence().AppendElement(
      WrapNotNull(MakeRefPtr<FileHandleWrapper>(DuplicateFileHandle(aReady))));
  UniqueFileHandle counter(
      eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE));
  MOZ_RELEASE_ASSERT(counter);
  desc.refCount().AppendElement(ipc::FileDescriptor(counter.get()));
  desc.foreignRGBImageState() = Some(
      layers::ForeignRGBImageState(1, WrapNotNull(RefPtr{aAccess.Handle()})));
  return desc;
}

static void CheckForeignRenderHost(void* aFixture, int aDrmFd, int aMemory,
                                   uint32_t aFormat, uint32_t aStride) {
  UniqueFileHandle ready(wr_test_foreign_rgb_ready(aFixture));
  ASSERT_TRUE(ready);
  for (bool abandon : {false, true}) {
    auto access = widget::DMABufAccess::Create();
    ASSERT_TRUE(access);
    auto desc = ForeignPublicationForTest(aMemory, ready.get(), aFormat,
                                          aStride, *access);
    RefPtr<DMABufSurface> surface = DMABufSurface::CreateDMABufSurface(desc);
    ASSERT_TRUE(surface);
    RefPtr<RenderDMABUFTextureHost> host = new RenderDMABUFTextureHost(surface);
    auto* first = wr_test_vulkan_renderer_new();
    auto* second = wr_test_vulkan_renderer_new();
    auto* context = wr_vulkan_external_images_new(first);
    auto* alias = wr_vulkan_external_images_new(first);
    auto* next = wr_vulkan_external_images_new(second);
    auto cleanup = MakeScopeExit([&] {
      wr_vulkan_external_images_delete(context);
      wr_vulkan_external_images_delete(alias);
      wr_vulkan_external_images_delete(next);
      wr_test_vulkan_renderer_delete(first);
      wr_test_vulkan_renderer_delete(second);
    });
    ASSERT_TRUE(context && alias && next);
    struct stat drm{};
    ASSERT_EQ(fstat(aDrmFd, &drm), 0);
    ASSERT_TRUE(S_ISCHR(drm.st_mode));
    const auto drmMajor = major(drm.st_rdev);
    const auto drmMinor = minor(drm.st_rdev);
    WrVulkanForeignRgbDescriptor supported{};
    supported.fd = -1;
    supported.width = 17;
    supported.height = 9;
    supported.fourcc = aFormat;
    supported.stride = aStride;
    EXPECT_FALSE(VulkanImageCapabilities::SupportsForeignRGB(
        supported, drmMajor, drmMinor));
    auto capabilities = VulkanImageCapabilities::Register(context);
    ASSERT_TRUE(capabilities);
    std::thread query([&] {
      EXPECT_TRUE(VulkanImageCapabilities::SupportsForeignRGB(
          supported, drmMajor, drmMinor));
      EXPECT_FALSE(VulkanImageCapabilities::SupportsForeignRGB(
          supported, drmMajor, UINT64_MAX));
      for (int change = 0; change < 5; ++change) {
        auto invalid = supported;
        switch (change) {
          case 0:
            invalid.fourcc = GBM_FORMAT_NV12;
            break;
          case 1:
            invalid.modifier = 1;
            break;
          case 2:
            invalid.width = INT32_MAX;
            invalid.stride = uint64_t(INT32_MAX) * 4;
            break;
          case 3:
            invalid.offset = 1;
            break;
          case 4:
            invalid.stride = 1;
            break;
        }
        EXPECT_FALSE(VulkanImageCapabilities::SupportsForeignRGB(
            invalid, drmMajor, drmMinor));
      }
    });
    query.join();
    VulkanImageReleaseQueue<> releases;
    ASSERT_TRUE(access->TryLock());
    EXPECT_EQ(host->LockVulkan(0, context).image_type,
              WrExternalImageType::Pending);
    EXPECT_FALSE(host->UnlockVulkan(context));
    EXPECT_TRUE(access->IsUsable());
    access->Unlock();
    const auto original = host->LockVulkan(0, context);
    ASSERT_EQ(original.image_type, WrExternalImageType::NativeTexture);
    const auto opaque = host->LockVulkan(1, alias);
    EXPECT_EQ(opaque.image_type, WrExternalImageType::NativeTexture);
    EXPECT_NE(original.handle, opaque.handle);
    EXPECT_FALSE(host->UnlockVulkan(alias));
    auto release = host->UnlockVulkan(context);
    ASSERT_TRUE(release);
    releases.Add(host, std::move(release.ref()));
    EXPECT_EQ(host->LockVulkan(1, context).handle, opaque.handle);
    release = host->UnlockVulkan(context);
    ASSERT_TRUE(release);
    releases.Add(host, std::move(release.ref()));
    releases.Poll();
    EXPECT_TRUE(releases.HasPending());
    EXPECT_FALSE(access->TryLock());
    EXPECT_EQ(host->LockVulkan(0, next).image_type,
              WrExternalImageType::Pending);
    EXPECT_FALSE(host->UnlockVulkan(next));
    EXPECT_TRUE(access->IsUsable());
    if (abandon) {
      releases.Poll(true);
      EXPECT_FALSE(access->IsUsable());
      EXPECT_EQ(host->LockVulkan(0, context).image_type,
                WrExternalImageType::Invalid);
      EXPECT_FALSE(host->UnlockVulkan(context));
      continue;
    }
    wr_test_vulkan_renderer_delete(first);
    first = nullptr;
    EXPECT_TRUE(VulkanImageCapabilities::SupportsForeignRGB(supported, drmMajor,
                                                            drmMinor));
    releases.Poll();
    EXPECT_FALSE(releases.HasPending());
    ASSERT_TRUE(access->TryLock());
    access->Unlock();
    EXPECT_EQ(host->LockVulkan(0, next).image_type,
              WrExternalImageType::NativeTexture);
    release = host->UnlockVulkan(next);
    ASSERT_TRUE(release);
    releases.Add(host, std::move(release.ref()));
    host = nullptr;
    EXPECT_FALSE(access->TryLock());
    wr_test_vulkan_renderer_delete(second);
    second = nullptr;
    releases.Poll();
    EXPECT_FALSE(releases.HasPending());
    ASSERT_TRUE(access->TryLock());
    access->Unlock();
  }
}

static void CheckForeignCompositorShutdown(void* aFixture, int aMemory,
                                           uint32_t aFormat, uint32_t aStride) {
  UniqueFileHandle ready(wr_test_foreign_rgb_ready(aFixture));
  ASSERT_TRUE(ready);
  for (bool cancel : {false, true}) {
    auto access = widget::DMABufAccess::Create();
    ASSERT_TRUE(access);
    auto desc = ForeignPublicationForTest(aMemory, ready.get(), aFormat,
                                          aStride, *access);
    RefPtr<DMABufSurface> surface = DMABufSurface::CreateDMABufSurface(desc);
    ASSERT_TRUE(surface);
    RefPtr<RenderDMABUFTextureHost> host = new RenderDMABUFTextureHost(surface);
    RenderCompositorVulkan compositor(nullptr, DetachedConfig());
    auto* renderer = wr_test_vulkan_renderer_new();
    const WindowId window{0};
    compositor.SetRenderer(renderer, window);
    auto cleanup = MakeScopeExit([&] {
      wr_test_vulkan_renderer_delete(renderer);
      compositor.SetRenderer(nullptr, window);
    });
    ASSERT_EQ(compositor.LockExternalImage(host, 0).image_type,
              WrExternalImageType::NativeTexture);
    compositor.UnlockExternalImage(host);
    compositor.AfterRender(true);
    EXPECT_FALSE(access->TryLock());
    if (cancel) {
      compositor.SetRenderer(nullptr, window);
    }
    wr_test_vulkan_renderer_delete(renderer);
    renderer = nullptr;
    compositor.SetRenderer(nullptr, window);
    EXPECT_EQ(access->IsUsable(), !cancel);
    EXPECT_EQ(access->TryLock(), !cancel);
    if (!cancel) {
      access->Unlock();
    }
  }
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanForeignRGBImport) {
  ASSERT_TRUE(widget::GbmLib::IsAvailable());
  OnRenderThread([] {
    int32_t drmFd = -1;
    auto* fixture = wr_test_foreign_rgb_new(&drmFd);
    ASSERT_TRUE(fixture);
    auto cleanup = MakeScopeExit([&] { wr_test_foreign_rgb_delete(fixture); });
    auto* gbm = widget::GbmLib::CreateDevice(drmFd);
    ASSERT_TRUE(gbm);
    auto destroyDevice =
        MakeScopeExit([&] { widget::GbmLib::DestroyDevice(gbm); });
    for (const auto format : {GBM_FORMAT_ARGB8888, GBM_FORMAT_ABGR8888}) {
      auto* buffer = widget::GbmLib::Create(
          gbm, 17, 9, format, GBM_BO_USE_WRITE | GBM_BO_USE_LINEAR);
      ASSERT_TRUE(buffer);
      auto destroyBuffer =
          MakeScopeExit([&] { widget::GbmLib::Destroy(buffer); });
      ASSERT_EQ(widget::GbmLib::GetModifier(buffer), 0u);
      ASSERT_EQ(widget::GbmLib::GetPlaneCount(buffer), 1);
      uint32_t mapStride = 0;
      void* mapData = nullptr;
      auto* pixels = static_cast<uint8_t*>(widget::GbmLib::Map(
          buffer, 0, 0, 17, 9, GBM_BO_TRANSFER_WRITE, &mapStride, &mapData));
      ASSERT_TRUE(pixels);
      for (size_t row = 0; row < 9; ++row) {
        memset(pixels + row * mapStride, 0x7f, 17 * 4);
      }
      widget::GbmLib::Unmap(buffer, mapData);
      UniqueFileHandle memory(widget::GbmLib::GetFd(buffer));
      ASSERT_TRUE(memory);
      wr_test_foreign_rgb_import(fixture, memory.get(), format,
                                 widget::GbmLib::GetStride(buffer));
      EXPECT_TRUE(DuplicateFileHandle(memory.get()));

      auto* renderer = wr_test_vulkan_renderer_new();
      auto deleteRenderer =
          MakeScopeExit([&] { wr_test_vulkan_renderer_delete(renderer); });
      auto* context = wr_vulkan_external_images_new(renderer);
      ASSERT_TRUE(context);
      auto deleteContext =
          MakeScopeExit([&] { wr_vulkan_external_images_delete(context); });
      WrVulkanForeignRgbDescriptor descriptor{};
      descriptor.fd = memory.get();
      descriptor.width = 17;
      descriptor.height = 9;
      descriptor.fourcc = format;
      descriptor.stride = widget::GbmLib::GetStride(buffer);
      auto* image = wr_vulkan_foreign_rgb_import(context, &descriptor);
      ASSERT_TRUE(image);
      auto deleteImage =
          MakeScopeExit([&] { wr_vulkan_foreign_rgb_delete(image); });
      EXPECT_TRUE(wr_vulkan_foreign_rgb_matches_context(image, context));
      UniqueFileHandle ready(wr_test_foreign_rgb_ready(fixture));
      ASSERT_TRUE(ready);
      ExternalTextureHandle handle{999};
      EXPECT_FALSE(wr_vulkan_foreign_rgb_acquire(image, -1, &handle));
      EXPECT_EQ(handle._0, 999u);
      ASSERT_TRUE(wr_vulkan_foreign_rgb_acquire(image, ready.get(), &handle));
      ExternalTextureHandle opaque{};
      ASSERT_TRUE(wr_vulkan_foreign_rgb_opaque_view(image, &opaque));
      EXPECT_NE(opaque._0, handle._0);
      auto* timeline = wr_vulkan_timeline_new(context);
      ASSERT_TRUE(timeline);
      auto deleteTimeline =
          MakeScopeExit([&] { wr_vulkan_timeline_delete(timeline); });
      auto* receipt = wr_vulkan_foreign_rgb_release(image, timeline, 1);
      ASSERT_TRUE(receipt);
      auto deleteReceipt =
          MakeScopeExit([&] { wr_vulkan_release_delete(receipt); });
      EXPECT_EQ(wr_vulkan_release_status(receipt),
                WrVulkanReleaseStatus::Pending);
      wr_vulkan_foreign_rgb_delete(image);
      image = nullptr;
      wr_vulkan_timeline_delete(timeline);
      timeline = nullptr;
      wr_vulkan_external_images_delete(context);
      context = nullptr;
      wr_test_vulkan_renderer_delete(renderer);
      renderer = nullptr;
      EXPECT_EQ(wr_vulkan_release_status(receipt),
                WrVulkanReleaseStatus::Complete);
      EXPECT_TRUE(DuplicateFileHandle(memory.get()));
      EXPECT_TRUE(DuplicateFileHandle(ready.get()));
      CheckForeignRenderHost(fixture, drmFd, memory.get(), format,
                             widget::GbmLib::GetStride(buffer));
      CheckForeignCompositorShutdown(fixture, memory.get(), format,
                                     widget::GbmLib::GetStride(buffer));
    }
  });
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanSyncFileWait) {
  OnRenderThread([] { wr_test_vulkan_sync_file_wait(); });
}

static void CheckVulkanPublication(bool aMigrate) {
  for (bool fail : {false, true}) {
    TestVulkanImage source{};
    auto* fixture = wr_test_vulkan_image_new(&source);
    ASSERT_NE(fixture, nullptr);
    auto cleanup = MakeScopeExit([&] { wr_test_vulkan_image_delete(fixture); });
    auto* renderer = wr_test_vulkan_image_renderer(fixture);
    auto* context = wr_vulkan_external_images_new(renderer);
    auto* alias = wr_vulkan_external_images_new(renderer);
    auto* next = aMigrate ? wr_vulkan_external_images_new(
        wr_test_vulkan_image_next_renderer(fixture)) : nullptr;
    ASSERT_NE(context, nullptr);
    ASSERT_NE(alias, nullptr);
    ASSERT_TRUE(!aMigrate || next);
    auto cleanupContexts = MakeScopeExit([&] {
      wr_vulkan_external_images_delete(context);
      wr_vulkan_external_images_delete(alias);
      wr_vulkan_external_images_delete(next);
    });
    auto capabilities = VulkanImageCapabilities::Register(context);
    ASSERT_TRUE(capabilities);
    WrVulkanDmaBufDescriptor supported{};
    supported.fd = -1;
    supported.width = supported.height = 2;
    supported.format = ImageFormat::RGBA8;
    supported.copy_src = source.mCopySrc;
    supported.copy_dst = true;
    std::copy_n(source.mDeviceUUID, 16, supported.device_uuid);
    std::copy_n(source.mDriverUUID, 16, supported.driver_uuid);
    std::thread query([&] {
      EXPECT_TRUE(VulkanImageCapabilities::Supports(supported));
      auto invalid = supported;
      invalid.device_uuid[0] ^= 1;
      EXPECT_FALSE(VulkanImageCapabilities::Supports(invalid));
      invalid = supported;
      invalid.driver_uuid[0] ^= 1;
      EXPECT_FALSE(VulkanImageCapabilities::Supports(invalid));
      invalid = supported;
      invalid.width = UINT32_MAX;
      EXPECT_FALSE(VulkanImageCapabilities::Supports(invalid));
      invalid = supported;
      invalid.modifier = UINT64_MAX;
      EXPECT_FALSE(VulkanImageCapabilities::Supports(invalid));
    });
    query.join();
    auto publication = PublicationForTest(source.mMemoryFd);
    publication.size() = IntSize(2, 2);
    publication.format() = SurfaceFormat::R8G8B8A8;
    publication.offset() = source.mOffset;
    publication.stride() = source.mStride;
    publication.copySrc() = source.mCopySrc;
    publication.colorTarget() = false;
    publication.copyDst() = true;
    publication.ready().handle() =
        new FileHandleWrapper(DuplicateFileHandle(source.mReadyFd));
    publication.ready().value() = 1;
    std::copy_n(source.mDeviceUUID, 16,
                publication.ready().deviceUUID().begin());
    std::copy_n(source.mDriverUUID, 16,
                publication.ready().driverUUID().begin());
    layers::VulkanImagePublication decoded;
    ASSERT_TRUE(RoundTripVulkanMessage(publication, decoded));
    Maybe<layers::VulkanImageReturnMessage> returned;
    RefPtr<layers::VulkanTextureHost> layersHost =
        layers::VulkanTextureHost::Create(
            layers::TextureFlags::DEFAULT, decoded,
            [&](layers::VulkanImageReturnMessage&& aReturn) {
              returned.emplace();
              EXPECT_TRUE(RoundTripVulkanMessage(aReturn, returned.ref()));
              EXPECT_TRUE(
                  ValidateVulkanImageReturn(returned.ref(), publication));
            });
    ASSERT_TRUE(layersHost);
    const auto id = layersHost->GetMaybeExternalImageId();
    ASSERT_TRUE(id);
    RefPtr<RenderTextureHost> host =
        RenderThread::Get()->GetRenderTexture(id.ref());
    ASSERT_TRUE(host);
    VulkanImageReleaseQueue<> releases;
    const auto first = host->LockVulkan(0, context);
    ASSERT_EQ(first.image_type, WrExternalImageType::NativeTexture);
    const auto nested = host->LockVulkan(1, alias);
    EXPECT_EQ(nested.image_type, WrExternalImageType::NativeTexture);
    EXPECT_NE(nested.handle, first.handle);
    EXPECT_FALSE(host->UnlockVulkan(context));
    auto release = host->UnlockVulkan(alias);
    ASSERT_TRUE(release);
    releases.Add(host, std::move(release.ref()));
    EXPECT_EQ(host->LockVulkan(1, alias).handle, nested.handle);
    release = host->UnlockVulkan(alias);
    ASSERT_TRUE(release);
    releases.Add(host, std::move(release.ref()));
    releases.Poll();
    EXPECT_FALSE(returned);
    if (fail) {
      EXPECT_EQ(host->LockVulkan(0, aMigrate ? next : nullptr).image_type,
                WrExternalImageType::Invalid);
      EXPECT_FALSE(host->UnlockVulkan(aMigrate ? next : nullptr));
    }
    if (aMigrate && !fail) {
      wr_test_vulkan_image_submit(fixture);
      releases.Poll();
      EXPECT_FALSE(releases.HasPending());
      EXPECT_FALSE(returned);
      const auto migrated = host->LockVulkan(0, next);
      ASSERT_EQ(migrated.image_type, WrExternalImageType::NativeTexture);
      wr_test_vulkan_image_sample(fixture, migrated.handle);
      release = host->UnlockVulkan(next);
      ASSERT_TRUE(release);
      releases.Add(host, std::move(release.ref()));
    }
    layersHost = nullptr;
    host = nullptr;
    EXPECT_FALSE(returned);
    wr_vulkan_external_images_delete(context);
    wr_vulkan_external_images_delete(alias);
    wr_vulkan_external_images_delete(next);
    context = alias = next = nullptr;
    if (!aMigrate || fail) {
      wr_test_vulkan_image_submit(fixture);
    }
    if (aMigrate) { wr_test_vulkan_image_submit_next(fixture); }
    releases.Poll();
    EXPECT_TRUE(VulkanImageCapabilities::Supports(supported));
    capabilities = nullptr;
    EXPECT_FALSE(VulkanImageCapabilities::Supports(supported));
    ASSERT_TRUE(returned);
    EXPECT_EQ(returned->status(), fail ? VulkanImageReturnStatus::Abandoned
                                       : VulkanImageReturnStatus::Submitted);
    if (!fail) {
      ASSERT_TRUE(returned->signal());
      const auto& signal = returned->signal().ref();
      EXPECT_EQ(signal.value(), aMigrate ? 1U : 2U);
      ASSERT_TRUE(signal.handle());
      EXPECT_TRUE(wr_test_vulkan_image_wait(
          fixture, signal.handle()->GetHandle(), signal.deviceUUID().data(),
          signal.driverUUID().data(), signal.value()));
      if (aMigrate) {
        EXPECT_EQ(wr_test_vulkan_image_value(
            fixture, signal.handle()->GetHandle(), signal.deviceUUID().data(),
            signal.driverUUID().data()), 1U);
      }
    }
  }
}

TEST_F(RenderExternalBuffer,
       DISABLED_VulkanPublicationReturnsSubmittedTimeline) {
  OnRenderThread([] {
    CheckVulkanPublication(false);
    CheckVulkanPublication(true);
  });
}
#  endif
#endif

}  // namespace mozilla::wr
