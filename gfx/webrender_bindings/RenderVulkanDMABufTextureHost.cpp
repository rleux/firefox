/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderVulkanDMABufTextureHost.h"

#include <algorithm>
#include <limits>
#include <memory>
#include <utility>

#include "VulkanImageRetirement.h"
#include "mozilla/CheckedInt.h"
#include "mozilla/webrender/RenderThread.h"

namespace mozilla::wr {

already_AddRefed<RenderVulkanDMABufTextureHost>
RenderVulkanDMABufTextureHost::Create(const WrVulkanDmaBufDescriptor& aImage,
                                      const WrVulkanTimelineDescriptor& aReady,
                                      uint64_t aReadyValue,
                                      ReturnCallback&& aReturn,
                                      uint64_t aPublicationId) {
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
      new RenderVulkanDMABufTextureHost(
          aImage, aReady, aReadyValue, bytes.value(), std::move(image),
          std::move(ready), std::move(aReturn), aPublicationId);
  return host.forget();
}

RenderVulkanDMABufTextureHost::RenderVulkanDMABufTextureHost(
    const WrVulkanDmaBufDescriptor& aImage,
    const WrVulkanTimelineDescriptor& aReady, uint64_t aReadyValue,
    size_t aBytes, UniqueFileHandle&& aImageFd, UniqueFileHandle&& aReadyFd,
    ReturnCallback&& aReturn, uint64_t aPublicationId)
    : mDescriptor(aImage),
      mReadyDescriptor(aReady),
      mImageFd(std::move(aImageFd)),
      mReadyFd(std::move(aReadyFd)),
      mReadyValue(aReadyValue),
      mBytes(aBytes),
      mPublicationId(aPublicationId),
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

WrVulkanRelease* RenderVulkanDMABufTextureHost::CurrentRelease(
    WrVulkanDmaBufImage* aImage, bool aBorrowed, uint64_t aLastIssued) {
  auto* receipt = wr_vulkan_dmabuf_last_release(aImage);
  if (aBorrowed &&
      (!receipt || wr_vulkan_release_value(receipt) <= aLastIssued)) {
    wr_vulkan_release_delete(receipt);
    receipt = wr_vulkan_dmabuf_release(aImage);
    if (!receipt) {
      receipt = wr_vulkan_dmabuf_last_release(aImage);
      if (receipt && wr_vulkan_release_value(receipt) <= aLastIssued) {
        wr_vulkan_release_delete(receipt);
        receipt = nullptr;
      }
    }
  }
  return receipt;
}

void RenderVulkanDMABufTextureHost::ReturnPublication() {
  if (!mReturn) {
    return;
  }
  auto* raw =
      mImage ? CurrentRelease(mImage.get(), mBorrowed || mAcquired, mLastIssued)
             : nullptr;
  auto callback = std::exchange(mReturn, nullptr);
  if (!raw && !mFailed && !mBorrowed && !mAcquired && !mLocks &&
      !mPendingReleases) {
    mReturnInfo.mStatus = VulkanImageReturnStatus::Unused;
    mReturnInfo.mValue = 0;
    mReturnInfo.mSemaphore.reset();
    auto info = std::make_shared<VulkanImageReturn>(std::move(mReturnInfo));
    RenderThread::Get()->NativeImages().Defer(
        [callback = std::move(callback), info]() mutable {
          callback(std::move(*info));
        });
    return;
  }
  auto info = std::make_shared<VulkanImageReturn>(std::move(mReturnInfo));
  auto finish = [callback = std::move(callback), info,
                 failed = mFailed || mLocks != 0](
                    uint64_t value, WrVulkanReleaseStatus status) mutable {
    if (!failed && status == WrVulkanReleaseStatus::Submitted && value &&
        info->mSemaphore) {
      info->mStatus = VulkanImageReturnStatus::Submitted;
      info->mValue = value;
    } else {
      info->mStatus = VulkanImageReturnStatus::Abandoned;
      info->mValue = 0;
      info->mSemaphore.reset();
    }
    callback(std::move(*info));
  };
  if (raw) {
    VulkanImageRelease release(raw, wr_vulkan_release_value(raw));
    RenderThread::Get()->NativeImages().Add(std::move(release),
                                            std::move(finish));
  } else if (mFailed || mBorrowed || mAcquired || mLocks || mPendingReleases) {
    RenderThread::Get()->NativeImages().Defer(
        [finish = std::move(finish)]() mutable {
          finish(0, WrVulkanReleaseStatus::Abandoned);
        });
  }
}

bool RenderVulkanDMABufTextureHost::Import(WrVulkanExternalImages* aImages) {
  mImage.reset(wr_vulkan_dmabuf_import(aImages, &mDescriptor));
  if (!mImage) {
    return false;
  }
  mReady.reset(wr_vulkan_timeline_import(aImages, &mReadyDescriptor));
  WrVulkanTimelineDescriptor exported{};
  if (!mReady || !wr_vulkan_dmabuf_return_timeline(mImage.get(), &exported)) {
    return false;
  }
  mReturnInfo.mSemaphore.reset(exported.fd);
  std::copy(std::begin(exported.device_uuid), std::end(exported.device_uuid),
            mReturnInfo.mDeviceUUID.begin());
  std::copy(std::begin(exported.driver_uuid), std::end(exported.driver_uuid),
            mReturnInfo.mDriverUUID.begin());
  return true;
}

bool RenderVulkanDMABufTextureHost::SwitchContext(
    WrVulkanExternalImages* aImages) {
  if (mLocks != 1 || mAcquired || mPendingReleases) {
    return false;
  }
  auto* raw = CurrentRelease(mImage.get(), mBorrowed, mLastIssued);
  if (raw) {
    VulkanImageRelease release(raw, wr_vulkan_release_value(raw));
    auto status = release.GetStatus();
    if (status == WrVulkanReleaseStatus::Pending) {
      return false;
    }
    if (status != WrVulkanReleaseStatus::Submitted) {
      mFailed = true;
      return false;
    }
    mReturnInfo.mStatus = VulkanImageReturnStatus::Submitted;
    mReturnInfo.mValue = release.GetValue();
  } else if (mBorrowed) {
    mFailed = true;
    return false;
  }
  if (mReturnInfo.mStatus == VulkanImageReturnStatus::Submitted) {
    mReadyFd = std::move(mReturnInfo.mSemaphore);
    mReadyDescriptor.fd = mReadyFd.get();
    mReadyValue = mReturnInfo.mValue;
  }
  mImage.reset();
  mReady.reset();
  mHandle = {};
  mOpaqueHandle = {};
  mLastIssued = 0;
  mLastNotified = 0;
  mReturnInfo = {};
  mBorrowed = false;
  return Import(aImages);
}

WrExternalImage RenderVulkanDMABufTextureHost::LockVulkan(
    uint8_t aChannelIndex, WrVulkanExternalImages* aImages) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  ++mLocks;
  if (mFailed || aChannelIndex > 1 || !aImages) {
    mFailed = true;
    return InvalidToWrExternalImage();
  }
  if (!mImage && !Import(aImages)) {
    mFailed = true;
    return InvalidToWrExternalImage();
  }
  if (!wr_vulkan_dmabuf_matches_context(mImage.get(), aImages) &&
      !SwitchContext(aImages)) {
    return mFailed ? InvalidToWrExternalImage() : PendingToWrExternalImage();
  }
  if (!mAcquired) {
    auto status = wr_vulkan_dmabuf_acquire(mImage.get(), mReady.get(),
                                           mReadyValue, &mHandle);
    if (status == WrVulkanAcquireStatus::Failed) {
      mFailed = true;
      return InvalidToWrExternalImage();
    }
    mBorrowed = wr_vulkan_dmabuf_has_access(mImage.get());
    if (status == WrVulkanAcquireStatus::Pending) {
      return PendingToWrExternalImage();
    }
    mAcquired = true;
  }
  if (aChannelIndex == 1 && !mOpaqueHandle._0 &&
      !wr_vulkan_dmabuf_opaque_view(mImage.get(), &mOpaqueHandle)) {
    mFailed = true;
    return InvalidToWrExternalImage();
  }
  auto handle = aChannelIndex ? mOpaqueHandle : mHandle;
  return NativeTextureToWrExternalImage(
      handle._0, 0, 0, float(mDescriptor.width), float(mDescriptor.height));
}

bool RenderVulkanDMABufTextureHost::CanReadbackVulkan(
    WrVulkanExternalImages* aImages, bool aAllowMigration) const {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (mFailed || !aImages) {
    return false;
  }
  if (mImage && wr_vulkan_dmabuf_matches_context(mImage.get(), aImages)) {
    return true;
  }
  if (!aAllowMigration) {
    return false;
  }
  auto* capabilities = wr_vulkan_dmabuf_capabilities_new(aImages);
  const bool supported = capabilities && wr_vulkan_dmabuf_capabilities_supports(
                                             capabilities, &mDescriptor);
  wr_vulkan_dmabuf_capabilities_delete(capabilities);
  return supported;
}

bool RenderVulkanDMABufTextureHost::ReadbackVulkan(uint8_t* aOutput,
                                                   size_t aLength) {
  return mAcquired && mImage && aLength == mBytes &&
         wr_vulkan_dmabuf_readback(mImage.get(), aOutput, aLength);
}

Maybe<VulkanImageRelease> RenderVulkanDMABufTextureHost::UnlockVulkan(
    WrVulkanExternalImages*) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (!mLocks) {
    mFailed = true;
  } else if (--mLocks) {
    return Nothing();
  }
  if (mFailed && !mPendingReleases && mReturn) {
    ++mPendingReleases;
    return Some(VulkanImageRelease(nullptr, 0));
  }
  if (!mAcquired || mFailed) {
    return Nothing();
  }
  mAcquired = false;
  mBorrowed = false;
  auto* raw = wr_vulkan_dmabuf_release(mImage.get());
  const auto value = wr_vulkan_release_value(raw);
  if (!raw || value <= mLastIssued) {
    mFailed = true;
  }
  mLastIssued = value;
  ++mPendingReleases;
  return Some(VulkanImageRelease(raw, value));
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
    ReturnPublication();
    return;
  }
  mReturnInfo.mStatus = VulkanImageReturnStatus::Submitted;
  mReturnInfo.mValue = aValue;
}

gfx::SurfaceFormat RenderVulkanDMABufTextureHost::GetFormat() const {
  return mDescriptor.format == ImageFormat::RGBA8
             ? gfx::SurfaceFormat::R8G8B8A8
             : gfx::SurfaceFormat::B8G8R8A8;
}

}  // namespace mozilla::wr
