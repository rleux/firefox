/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderVulkanDMABufTextureHost.h"

#include <algorithm>
#include <limits>
#include <utility>

#include "mozilla/CheckedInt.h"
#include "mozilla/webrender/RenderThread.h"

namespace mozilla::wr {

already_AddRefed<RenderVulkanDMABufTextureHost>
RenderVulkanDMABufTextureHost::Create(const WrVulkanDmaBufDescriptor& aImage,
                                      const WrVulkanTimelineDescriptor& aReady,
                                      uint64_t aReadyValue,
                                      ReturnCallback&& aReturn) {
  auto bytes = CheckedInt<size_t>(aImage.width) * aImage.height * 4;
  if (!aReturn || !aReadyValue || !aImage.width || !aImage.height ||
      aImage.width > INT32_MAX || aImage.height > INT32_MAX || !aImage.stride ||
      !bytes.isValid() ||
      (aImage.format != ImageFormat::RGBA8 &&
       aImage.format != ImageFormat::BGRA8) ||
      !std::equal(std::begin(aImage.device_uuid), std::end(aImage.device_uuid),
                  std::begin(aReady.device_uuid)) ||
      !std::equal(std::begin(aImage.driver_uuid), std::end(aImage.driver_uuid),
                  std::begin(aReady.driver_uuid))) {
    return nullptr;
  }
  auto image = DuplicateFileHandle(aImage.fd);
  auto ready = DuplicateFileHandle(aReady.fd);
  if (!image || !ready) {
    return nullptr;
  }
  RefPtr<RenderVulkanDMABufTextureHost> host =
      new RenderVulkanDMABufTextureHost(aImage, aReady, aReadyValue,
                                        bytes.value(), std::move(image),
                                        std::move(ready), std::move(aReturn));
  return host.forget();
}

RenderVulkanDMABufTextureHost::RenderVulkanDMABufTextureHost(
    const WrVulkanDmaBufDescriptor& aImage,
    const WrVulkanTimelineDescriptor& aReady, uint64_t aReadyValue,
    size_t aBytes, UniqueFileHandle&& aImageFd, UniqueFileHandle&& aReadyFd,
    ReturnCallback&& aReturn)
    : mDescriptor(aImage),
      mReadyDescriptor(aReady),
      mImageFd(std::move(aImageFd)),
      mReadyFd(std::move(aReadyFd)),
      mReadyValue(aReadyValue),
      mBytes(aBytes),
      mReturn(std::move(aReturn)) {
  MOZ_COUNT_CTOR_INHERITED(RenderVulkanDMABufTextureHost, RenderTextureHost);
  mDescriptor.fd = mImageFd.get();
  mReadyDescriptor.fd = mReadyFd.get();
}

RenderVulkanDMABufTextureHost::~RenderVulkanDMABufTextureHost() {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  MOZ_COUNT_DTOR_INHERITED(RenderVulkanDMABufTextureHost, RenderTextureHost);
  ReturnPublication();
}

void RenderVulkanDMABufTextureHost::ReturnPublication() {
  if (!mReturn) {
    return;
  }
  if (mFailed || mAcquired || mLocks || mPendingReleases) {
    mReturnInfo.mStatus = VulkanImageReturnStatus::Abandoned;
  }
  mImage.reset();
  mReady.reset();
  mReleased.reset();
  mImageFd.reset();
  mReadyFd.reset();
  auto callback = std::exchange(mReturn, nullptr);
  callback(std::move(mReturnInfo));
}

bool RenderVulkanDMABufTextureHost::Import(WrVulkanExternalImages* aImages) {
  mImage.reset(wr_vulkan_dmabuf_import(aImages, &mDescriptor));
  if (!mImage) {
    return false;
  }
  mReady.reset(wr_vulkan_timeline_import(aImages, &mReadyDescriptor));
  mReleased.reset(wr_vulkan_timeline_new(aImages));
  WrVulkanTimelineDescriptor exported{};
  if (!mReady || !mReleased ||
      !wr_vulkan_timeline_export(mReleased.get(), &exported)) {
    return false;
  }
  mReturnInfo.mSemaphore.reset(exported.fd);
  std::copy(std::begin(exported.device_uuid), std::end(exported.device_uuid),
            mReturnInfo.mDeviceUUID.begin());
  std::copy(std::begin(exported.driver_uuid), std::end(exported.driver_uuid),
            mReturnInfo.mDriverUUID.begin());
  return true;
}

WrExternalImage RenderVulkanDMABufTextureHost::LockVulkan(
    uint8_t aChannelIndex, WrVulkanExternalImages* aImages) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  ++mLocks;
  if (mFailed || aChannelIndex || !aImages) {
    mFailed = true;
    return InvalidToWrExternalImage();
  }
  if ((!mImage && !Import(aImages)) ||
      !wr_vulkan_dmabuf_matches_context(mImage.get(), aImages)) {
    mFailed = true;
    return InvalidToWrExternalImage();
  }
  if (!mAcquired) {
    if (!wr_vulkan_dmabuf_acquire(mImage.get(), mReady.get(), mReadyValue,
                                  &mHandle)) {
      mFailed = true;
      return InvalidToWrExternalImage();
    }
    mAcquired = true;
  }
  return NativeTextureToWrExternalImage(
      mHandle._0, 0, 0, float(mDescriptor.width), float(mDescriptor.height));
}

Maybe<VulkanImageRelease> RenderVulkanDMABufTextureHost::UnlockVulkan(
    WrVulkanExternalImages*) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (!mLocks) {
    mFailed = true;
  } else if (--mLocks) {
    return Nothing();
  }
  if (mLastIssued == std::numeric_limits<uint64_t>::max()) {
    mFailed = true;
  }
  if (mFailed && !mPendingReleases && mReturn) {
    ++mPendingReleases;
    return Some(VulkanImageRelease(nullptr, 0));
  }
  if (!mAcquired || mFailed) {
    return Nothing();
  }
  mAcquired = false;
  const auto value = ++mLastIssued;
  auto* receipt =
      wr_vulkan_dmabuf_release(mImage.get(), mReleased.get(), value);
  ++mPendingReleases;
  return Some(VulkanImageRelease(receipt, value));
}

void RenderVulkanDMABufTextureHost::NotifyVulkanRelease(
    uint64_t aValue, WrVulkanReleaseStatus aStatus) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (aValue == 0 && mFailed && mPendingReleases) {
    --mPendingReleases;
    ReturnPublication();
    return;
  }
  if (!mPendingReleases || aValue <= mLastNotified || aValue > mLastIssued) {
    mFailed = true;
    ReturnPublication();
    return;
  }
  --mPendingReleases;
  mLastNotified = aValue;
  if (aStatus != WrVulkanReleaseStatus::Submitted) {
    mFailed = true;
  } else {
    mReturnInfo.mStatus = VulkanImageReturnStatus::Submitted;
    mReturnInfo.mValue = aValue;
  }
  if (mFailed) {
    ReturnPublication();
    return;
  }
}

gfx::SurfaceFormat RenderVulkanDMABufTextureHost::GetFormat() const {
  return mDescriptor.format == ImageFormat::RGBA8
             ? gfx::SurfaceFormat::R8G8B8A8
             : gfx::SurfaceFormat::B8G8R8A8;
}

}  // namespace mozilla::wr
