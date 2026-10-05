/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <algorithm>
#include <array>
#include <cstring>
#include <functional>
#include <thread>
#include <vector>

#include "gfxPlatform.h"
#include "gtest/gtest.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/GPUProcessManager.h"
#include "mozilla/layers/RemoteTextureMap.h"
#include "mozilla/layers/SourceSurfaceSharedData.h"
#include "mozilla/layers/SynchronousTask.h"
#include "mozilla/layers/VulkanImages.h"
#include "mozilla/layers/VulkanTextureHost.h"
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
#  include <sys/syscall.h>
#  include <unistd.h>

#  include "base/linux_memfd_defs.h"

#  include "mozilla/webgpu/SharedTextureVulkan.h"
#  include "mozilla/webrender/RenderDMABUFTextureHost.h"
#  include "mozilla/webrender/RenderVulkanDMABufTextureHost.h"
#  include "mozilla/widget/DMABufDevice.h"
#  include "mozilla/widget/DMABufAccess.h"
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
void wr_test_vulkan_image_submit(void*);
bool wr_test_vulkan_image_wait(void*, int32_t, const uint8_t*, const uint8_t*,
                               uint64_t);
void wr_test_vulkan_image_delete(void*);
void* wr_test_webgpu_image_new(TestVulkanImage*);
Renderer* wr_test_webgpu_image_renderer(void*);
void wr_test_webgpu_image_submit(void*);
bool wr_test_webgpu_image_wait(void*, int32_t, const uint8_t*, const uint8_t*,
                               uint64_t);
void wr_test_webgpu_image_delete(void*);
bool wr_test_webgpu_read_transition_initializes();
void wr_test_webgpu_unused_publication_recycles();
void wr_test_webgpu_timeline_lifecycle();
void wr_test_webgpu_timeline_submission();
void wr_test_webgpu_timeline_failed_submission();
void wr_test_webgpu_dmabuf_allocation();
bool wr_test_webgpu_import_initialization();
void wr_test_webgpu_dmabuf_import();
void wr_test_webgpu_global_init(const webgpu::ffi::WGPUGlobal*);
Renderer* wr_test_vulkan_renderer_new();
void wr_test_vulkan_renderer_delete(Renderer*);
void wr_test_vulkan_sync_file_wait();
void* wr_test_foreign_rgb_new(int32_t*);
void wr_test_foreign_rgb_import(void*, int32_t, uint32_t, uint64_t);
void wr_test_foreign_rgb_delete(void*);
int32_t wr_test_foreign_rgb_ready(void*);
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUSharedTextureVulkanLifecycle) {
  OnRenderThread([] {
    namespace ffi = webgpu::ffi;
    using Texture = webgpu::SharedTextureVulkan;
    auto* global = ffi::wgpu_server_new(nullptr);
    wr_test_webgpu_global_init(global);
    auto cleanup = MakeScopeExit([&] {
      ffi::wgpu_server_poll_all_devices(global, true);
      ffi::wgpu_server_delete(global);
    });
    ffi::WGPUTextureFormat format{};
    format.tag = ffi::WGPUTextureFormat_Rgba8Unorm;
    auto usage = WGPUTextureUsages_COPY_DST |
                 WGPUTextureUsages_RENDER_ATTACHMENT;
    auto texture = Texture::Create(global, 1, 2, 2, format, usage);
    ASSERT_TRUE(texture);
    EXPECT_EQ(texture->GetAcquireTimeline(), nullptr);
    EXPECT_EQ(texture->GetAcquireValue(), 0u);
    ffi::WGPUFfiTextureDescriptor desc{};
    desc.size = {2, 2, 1};
    desc.mip_level_count = 1;
    desc.sample_count = 1;
    desc.dimension = ffi::WGPUTextureDimension_D2;
    desc.format = format;
    desc.usage = usage;
    for (uint64_t id = 1; id <= 3; ++id) {
      auto memory = texture->CloneDmaBufFd();
      ASSERT_TRUE(ffi::wgpu_vkimage_import_for_webrender(
          global, 1, id, &desc, memory.get(), &texture->GetDMABufInfo(),
          texture->GetAcquireTimeline(), texture->GetAcquireValue()));
      ASSERT_TRUE(texture->Publish(global, 1, id, id));
      EXPECT_TRUE(texture->IsSubmitted());
      EXPECT_TRUE(ValidateVulkanImagePublication(texture->GetPublication()));
      EXPECT_EQ(texture->TryRecycle(global), Texture::RecycleStatus::Pending);
      auto callback = texture->GetReturnCallback();
      callback(layers::VulkanImageReturnMessage(
          id, VulkanImageReturnStatus::Unused, Nothing()));
      ASSERT_EQ(texture->TryRecycle(global), Texture::RecycleStatus::Ready);
      EXPECT_FALSE(texture->IsSubmitted());
      EXPECT_NE(texture->GetAcquireTimeline(), nullptr);
      EXPECT_EQ(texture->GetAcquireValue(), id);
    }
    auto memory = texture->CloneDmaBufFd();
    ASSERT_TRUE(ffi::wgpu_vkimage_import_for_webrender(
        global, 1, 4, &desc, memory.get(), &texture->GetDMABufInfo(),
        texture->GetAcquireTimeline(), texture->GetAcquireValue()));
    ASSERT_TRUE(texture->Publish(global, 1, 4, 4));
    auto callback = texture->GetReturnCallback();
    callback(layers::VulkanImageReturnMessage(
        3, VulkanImageReturnStatus::Unused, Nothing()));
    EXPECT_EQ(texture->TryRecycle(global), Texture::RecycleStatus::Abandoned);
    texture = nullptr;
    callback(layers::VulkanImageReturnMessage(
        4, VulkanImageReturnStatus::Abandoned, Nothing()));
  });
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

static void CheckForeignRenderHost(void* aFixture, int aMemory,
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
          gbm, 17, 9, format, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
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
      CheckForeignRenderHost(fixture, memory.get(), format,
                             widget::GbmLib::GetStride(buffer));
      CheckForeignCompositorShutdown(fixture, memory.get(), format,
                                     widget::GbmLib::GetStride(buffer));
    }
  });
}

TEST_F(RenderExternalBuffer, DISABLED_VulkanSyncFileWait) {
  OnRenderThread([] { wr_test_vulkan_sync_file_wait(); });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUSharedTimelineLifecycle) {
  OnRenderThread([] { wr_test_webgpu_timeline_lifecycle(); });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUSharedTimelineSubmission) {
  OnRenderThread([] { wr_test_webgpu_timeline_submission(); });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUSharedTimelineFailedSubmission) {
  OnRenderThread([] { wr_test_webgpu_timeline_failed_submission(); });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUSharedDMABufAllocation) {
  OnRenderThread([] { wr_test_webgpu_dmabuf_allocation(); });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUImportedTextureInitialization) {
  OnRenderThread([] { EXPECT_TRUE(wr_test_webgpu_import_initialization()); });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUSharedDMABufImport) {
  OnRenderThread([] { wr_test_webgpu_dmabuf_import(); });
}

static void CheckVulkanPublication(bool aWebGPU) {
  const auto create =
      aWebGPU ? wr_test_webgpu_image_new : wr_test_vulkan_image_new;
  const auto rendererFor =
      aWebGPU ? wr_test_webgpu_image_renderer : wr_test_vulkan_image_renderer;
  const auto submit =
      aWebGPU ? wr_test_webgpu_image_submit : wr_test_vulkan_image_submit;
  const auto wait =
      aWebGPU ? wr_test_webgpu_image_wait : wr_test_vulkan_image_wait;
  const auto destroy =
      aWebGPU ? wr_test_webgpu_image_delete : wr_test_vulkan_image_delete;
  for (bool fail : {false, true}) {
    TestVulkanImage source{};
    auto* fixture = create(&source);
    ASSERT_NE(fixture, nullptr);
    auto cleanup = MakeScopeExit([&] { destroy(fixture); });
    auto* renderer = rendererFor(fixture);
    auto* context = wr_vulkan_external_images_new(renderer);
    auto* alias = wr_vulkan_external_images_new(renderer);
    ASSERT_NE(context, nullptr);
    ASSERT_NE(alias, nullptr);
    auto cleanupContexts = MakeScopeExit([&] {
      wr_vulkan_external_images_delete(context);
      wr_vulkan_external_images_delete(alias);
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
      EXPECT_EQ(host->LockVulkan(0, nullptr).image_type,
                WrExternalImageType::Invalid);
      EXPECT_FALSE(host->UnlockVulkan(nullptr));
    }
    layersHost = nullptr;
    host = nullptr;
    EXPECT_FALSE(returned);
    wr_vulkan_external_images_delete(context);
    wr_vulkan_external_images_delete(alias);
    context = alias = nullptr;
    submit(fixture);
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
      EXPECT_EQ(signal.value(), 2U);
      ASSERT_TRUE(signal.handle());
      EXPECT_TRUE(wait(fixture, signal.handle()->GetHandle(),
                       signal.deviceUUID().data(), signal.driverUUID().data(),
                       signal.value()));
    }
  }
}

TEST_F(RenderExternalBuffer,
       DISABLED_VulkanPublicationReturnsSubmittedTimeline) {
  OnRenderThread([] { CheckVulkanPublication(false); });
}
TEST_F(RenderExternalBuffer,
       DISABLED_WebGPUPublicationReturnsSubmittedTimeline) {
  OnRenderThread([] { CheckVulkanPublication(true); });
}
TEST_F(RenderExternalBuffer, DISABLED_WebGPUImageChangesRenderer) {
  OnRenderThread([] {
    TestVulkanImage source{};
    auto* fixture = wr_test_webgpu_image_new(&source);
    ASSERT_TRUE(fixture);
    auto cleanup = MakeScopeExit([&] { wr_test_webgpu_image_delete(fixture); });
    auto* second = wr_test_vulkan_renderer_new();
    auto cleanupSecond =
        MakeScopeExit([&] { wr_test_vulkan_renderer_delete(second); });
    auto* firstContext =
        wr_vulkan_external_images_new(wr_test_webgpu_image_renderer(fixture));
    auto* secondContext = wr_vulkan_external_images_new(second);
    ASSERT_TRUE(firstContext && secondContext);
    auto cleanupContexts = MakeScopeExit([&] {
      wr_vulkan_external_images_delete(firstContext);
      wr_vulkan_external_images_delete(secondContext);
    });
    auto publication = PublicationForTest(source.mMemoryFd);
    publication.size() = IntSize(2, 2);
    publication.format() = SurfaceFormat::R8G8B8A8;
    publication.offset() = source.mOffset;
    publication.stride() = source.mStride;
    publication.copySrc() = source.mCopySrc;
    publication.copyDst() = true;
    publication.colorTarget() = false;
    publication.ready().handle() =
        new FileHandleWrapper(DuplicateFileHandle(source.mReadyFd));
    publication.ready().value() = 1;
    std::copy_n(source.mDeviceUUID, 16,
                publication.ready().deviceUUID().begin());
    std::copy_n(source.mDriverUUID, 16,
                publication.ready().driverUUID().begin());
    Maybe<layers::VulkanImageReturnMessage> returned;
    RefPtr<RenderTextureHost> host = CreateVulkanImageHost(
        publication, [&](layers::VulkanImageReturnMessage&& aReturn) {
          returned.emplace(std::move(aReturn));
        });
    ASSERT_TRUE(host);
    VulkanImageReleaseQueue<> releases;
    for (size_t i = 0; i < 2; ++i) {
      ASSERT_EQ(host->LockVulkan(1, firstContext).image_type,
                WrExternalImageType::NativeTexture);
      auto release = host->UnlockVulkan(firstContext);
      ASSERT_TRUE(release);
      releases.Add(host, std::move(release.ref()));
    }
    wr_test_webgpu_image_submit(fixture);
    releases.Poll();
    EXPECT_FALSE(returned);
    const auto opaque = host->LockVulkan(1, secondContext);
    ASSERT_EQ(opaque.image_type, WrExternalImageType::NativeTexture);
    const auto alpha = host->LockVulkan(0, secondContext);
    EXPECT_EQ(alpha.image_type, WrExternalImageType::NativeTexture);
    EXPECT_NE(alpha.handle, opaque.handle);
    EXPECT_FALSE(host->UnlockVulkan(secondContext));
    auto release = host->UnlockVulkan(secondContext);
    ASSERT_TRUE(release);
    releases.Add(host, std::move(release.ref()));
    host = nullptr;
    wr_vulkan_external_images_delete(firstContext);
    wr_vulkan_external_images_delete(secondContext);
    firstContext = secondContext = nullptr;
    wr_test_vulkan_renderer_delete(second);
    second = nullptr;
    releases.Poll();
    ASSERT_TRUE(returned);
    EXPECT_TRUE(ValidateVulkanImageReturn(returned.ref(), publication));
    ASSERT_EQ(returned->status(), VulkanImageReturnStatus::Submitted);
    ASSERT_TRUE(returned->signal());
    const auto& signal = returned->signal().ref();
    EXPECT_EQ(signal.value(), 1u);
    EXPECT_TRUE(wr_test_webgpu_image_wait(
        fixture, signal.handle()->GetHandle(), signal.deviceUUID().data(),
        signal.driverUUID().data(), signal.value()));
  });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUReadTransitionInitializes) {
  OnRenderThread(
      [] { EXPECT_TRUE(wr_test_webgpu_read_transition_initializes()); });
}

TEST_F(RenderExternalBuffer, DISABLED_WebGPUUnusedPublicationRecycles) {
  OnRenderThread([] { wr_test_webgpu_unused_publication_recycles(); });
}

#  endif
#endif

}  // namespace mozilla::wr
