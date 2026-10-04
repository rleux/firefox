/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "SharedTextureVulkan.h"

#include <algorithm>
#include <limits>

#include "mozilla/webrender/VulkanImageIPC.h"

namespace mozilla::webgpu {

UniquePtr<SharedTextureVulkan> SharedTextureVulkan::Create(
    const ffi::WGPUGlobal* aContext, ffi::WGPUDeviceId aDeviceId,
    uint32_t aWidth, uint32_t aHeight, ffi::WGPUTextureFormat aFormat,
    ffi::WGPUTextureUsages aUsage) {
  UniquePtr<SharedTextureVulkan> texture(
      new SharedTextureVulkan(aDeviceId, aWidth, aHeight, aFormat, aUsage));
  int32_t fd = -1;
  texture->mInfo = ffi::wgpu_vkimage_create_for_webrender(
      aContext, aDeviceId, aWidth, aHeight, aFormat, aUsage, &fd);
  UniqueFileHandle memory(fd);
  if (!texture->mInfo.layout.is_valid || !memory) {
    return nullptr;
  }
  texture->mMemory = new gfx::FileHandleWrapper(std::move(memory));
  texture->mReady.reset(ffi::wgpu_vulkan_timeline_new(aContext, aDeviceId));
  ffi::WGPUVulkanTimelineDescriptor ready{};
  ready.fd = -1;
  if (!ffi::wgpu_vulkan_timeline_export(texture->mReady.get(), &ready)) {
    return nullptr;
  }
  texture->mReadyDescriptor.handle() =
      new gfx::FileHandleWrapper(UniqueFileHandle(ready.fd));
  std::copy(std::begin(ready.device_uuid), std::end(ready.device_uuid),
            texture->mReadyDescriptor.deviceUUID().begin());
  std::copy(std::begin(ready.driver_uuid), std::end(ready.driver_uuid),
            texture->mReadyDescriptor.driverUUID().begin());
  texture->mReadyDescriptor.value() = 0;
  return texture;
}

SharedTextureVulkan::SharedTextureVulkan(ffi::WGPUDeviceId aDeviceId,
                                         uint32_t aWidth, uint32_t aHeight,
                                         ffi::WGPUTextureFormat aFormat,
                                         ffi::WGPUTextureUsages aUsage)
    : SharedTexture(aWidth, aHeight, aFormat, aUsage), mDeviceId(aDeviceId) {}

SharedTextureVulkan::~SharedTextureVulkan() = default;

UniqueFileHandle SharedTextureVulkan::CloneDmaBufFd() const {
  return mMemory->ClonePlatformHandle();
}

const ffi::WGPUVulkanTimeline* SharedTextureVulkan::GetAcquireTimeline() const {
  return mAcquireValue ? (mReturned ? mReturned.get() : mReady.get()) : nullptr;
}

bool SharedTextureVulkan::Publish(const ffi::WGPUGlobal* aContext,
                                  ffi::WGPUQueueId aQueueId,
                                  ffi::WGPUTextureId aTextureId,
                                  uint64_t aPublicationId) {
  MOZ_ASSERT(!mPublication && !mReturn);
  if (!aPublicationId ||
      mReadyDescriptor.value() == std::numeric_limits<uint64_t>::max()) {
    return false;
  }
  ++mReadyDescriptor.value();
  auto submission = ffi::wgpu_vkimage_release_for_webrender(
      aContext, aQueueId, aTextureId, mReady.get(), mReadyDescriptor.value());
  if (!submission) {
    return false;
  }
  SetSubmissionIndex(submission);
  mReturned = nullptr;
  mAcquireValue = 0;
  mPublication.emplace(
      aPublicationId, mMemory, GetSize(),
      mInfo.rgba ? gfx::SurfaceFormat::R8G8B8A8 : gfx::SurfaceFormat::B8G8R8A8,
      mInfo.layout.modifier, mInfo.layout.offsets[0], mInfo.layout.strides[0],
      mInfo.copy_src, mInfo.copy_dst, mInfo.color_target, mReadyDescriptor);
  mReturn = std::make_shared<ReturnState>();
  return true;
}

std::function<void(layers::VulkanImageReturnMessage&&)>
SharedTextureVulkan::GetReturnCallback() {
  MOZ_ASSERT(mReturn);
  return [state = mReturn](layers::VulkanImageReturnMessage&& aResult) {
    MutexAutoLock lock(state->mMutex);
    if (state->mResult) {
      state->mResult->status() = wr::VulkanImageReturnStatus::Abandoned;
      state->mResult->signal().reset();
    } else {
      state->mResult.emplace(std::move(aResult));
    }
  };
}

SharedTextureVulkan::RecycleStatus SharedTextureVulkan::TryRecycle(
    const ffi::WGPUGlobal* aContext) {
  MOZ_ASSERT(mReturn && mPublication);
  Maybe<layers::VulkanImageReturnMessage> result;
  {
    MutexAutoLock lock(mReturn->mMutex);
    if (!mReturn->mResult) {
      return RecycleStatus::Pending;
    }
    result = std::move(mReturn->mResult);
  }
  if (!wr::ValidateVulkanImageReturn(result.ref(), mPublication.ref()) ||
      result->status() == wr::VulkanImageReturnStatus::Abandoned) {
    return RecycleStatus::Abandoned;
  }
  if (result->status() == wr::VulkanImageReturnStatus::Submitted) {
    const auto& signal = result->signal().ref();
    ffi::WGPUVulkanTimelineDescriptor descriptor{};
    descriptor.fd = signal.handle()->GetHandle();
    std::copy(signal.deviceUUID().begin(), signal.deviceUUID().end(),
              descriptor.device_uuid);
    std::copy(signal.driverUUID().begin(), signal.driverUUID().end(),
              descriptor.driver_uuid);
    mReturned.reset(
        ffi::wgpu_vulkan_timeline_import(aContext, mDeviceId, &descriptor));
    if (!mReturned) {
      return RecycleStatus::Abandoned;
    }
    mAcquireValue = signal.value();
  } else {
    mAcquireValue = mReadyDescriptor.value();
  }
  mPublication.reset();
  mReturn.reset();
  CleanForRecycling();
  return RecycleStatus::Ready;
}

}  // namespace mozilla::webgpu
