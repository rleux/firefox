/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_RENDERVULKANDMABUFTEXTUREHOST_H
#define MOZILLA_GFX_RENDERVULKANDMABUFTEXTUREHOST_H

#include <functional>

#include "RenderTextureHost.h"
#include "VulkanImageTypes.h"

namespace mozilla::wr {

// A successful Create borrows immutable image contents until the return
// callback. Submitted returns require a GPU wait before reuse; Abandoned
// allocations must be discarded. FDs are duplicated; the caller retains its
// original descriptors.
class RenderVulkanDMABufTextureHost final : public RenderTextureHost {
 public:
  using ReturnCallback = std::function<void(VulkanImageReturn&&)>;
  static already_AddRefed<RenderVulkanDMABufTextureHost> Create(
      const WrVulkanDmaBufDescriptor& aImage,
      const WrVulkanTimelineDescriptor& aReady, uint64_t aReadyValue,
      ReturnCallback&& aReturn);

  // Channels 0 and 1 sample the original alpha and constant-one alpha.
  WrExternalImage LockVulkan(uint8_t aChannelIndex,
                             WrVulkanExternalImages* aImages) override;
  Maybe<VulkanImageRelease> UnlockVulkan(
      WrVulkanExternalImages* aImages) override;
  void NotifyVulkanRelease(uint64_t aValue,
                           WrVulkanReleaseStatus aStatus) override;
  size_t Bytes() override { return mBytes; }
  gfx::SurfaceFormat GetFormat() const override;

 private:
  RenderVulkanDMABufTextureHost(const WrVulkanDmaBufDescriptor& aImage,
                                const WrVulkanTimelineDescriptor& aReady,
                                uint64_t aReadyValue, size_t aBytes,
                                UniqueFileHandle&& aImageFd,
                                UniqueFileHandle&& aReadyFd,
                                ReturnCallback&& aReturn);
  ~RenderVulkanDMABufTextureHost() override;
  bool Import(WrVulkanExternalImages* aImages);
  void ReturnPublication();

  struct ImageDeleter {
    void operator()(WrVulkanDmaBufImage* aImage) {
      wr_vulkan_dmabuf_delete(aImage);
    }
  };
  struct TimelineDeleter {
    void operator()(WrVulkanTimeline* aTimeline) {
      wr_vulkan_timeline_delete(aTimeline);
    }
  };
  UniquePtr<WrVulkanDmaBufImage, ImageDeleter> mImage;
  UniquePtr<WrVulkanTimeline, TimelineDeleter> mReady;
  UniquePtr<WrVulkanTimeline, TimelineDeleter> mReleased;
  WrVulkanDmaBufDescriptor mDescriptor;
  WrVulkanTimelineDescriptor mReadyDescriptor;
  UniqueFileHandle mImageFd;
  UniqueFileHandle mReadyFd;
  const uint64_t mReadyValue;
  const size_t mBytes;
  ReturnCallback mReturn;
  VulkanImageReturn mReturnInfo;
  ExternalTextureHandle mHandle{};
  ExternalTextureHandle mOpaqueHandle{};
  uint64_t mLastIssued = 0;
  uint64_t mLastNotified = 0;
  size_t mPendingReleases = 0;
  size_t mLocks = 0;
  bool mAcquired = false;
  bool mFailed = false;
};

}  // namespace mozilla::wr

#endif
