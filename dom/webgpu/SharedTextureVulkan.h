/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef GPU_SharedTextureVulkan_H_
#define GPU_SharedTextureVulkan_H_

#include <functional>
#include <memory>

#include "mozilla/Mutex.h"
#include "mozilla/layers/VulkanImages.h"
#include "mozilla/webgpu/SharedTexture.h"

namespace mozilla::webgpu {

class SharedTextureVulkan final : public SharedTexture {
 public:
  static UniquePtr<SharedTextureVulkan> Create(const ffi::WGPUGlobal* aContext,
                                               ffi::WGPUDeviceId aDeviceId,
                                               uint32_t aWidth,
                                               uint32_t aHeight,
                                               ffi::WGPUTextureFormat aFormat,
                                               ffi::WGPUTextureUsages aUsage);

  ~SharedTextureVulkan() override;

  Maybe<layers::SurfaceDescriptor> ToSurfaceDescriptor() override {
    return Nothing();
  }
  SharedTextureVulkan* AsSharedTextureVulkan() override { return this; }

  UniqueFileHandle CloneDmaBufFd() const;
  const ffi::WGPUVulkanDmaBufInfo& GetDMABufInfo() const { return mInfo; }
  const ffi::WGPUVulkanTimeline* GetAcquireTimeline() const;
  uint64_t GetAcquireValue() const { return mAcquireValue; }

  bool Publish(const ffi::WGPUGlobal* aContext, ffi::WGPUQueueId aQueueId,
               ffi::WGPUTextureId aTextureId, uint64_t aPublicationId);
  const layers::VulkanImagePublication& GetPublication() const {
    return mPublication.ref();
  }
  std::function<void(layers::VulkanImageReturnMessage&&)> GetReturnCallback();

  enum class RecycleStatus { Pending, Ready, Abandoned };
  RecycleStatus TryRecycle(const ffi::WGPUGlobal* aContext);

 private:
  SharedTextureVulkan(ffi::WGPUDeviceId aDeviceId, uint32_t aWidth,
                      uint32_t aHeight, ffi::WGPUTextureFormat aFormat,
                      ffi::WGPUTextureUsages aUsage);

  struct TimelineDeleter {
    void operator()(ffi::WGPUVulkanTimeline* aTimeline) {
      ffi::wgpu_vulkan_timeline_delete(aTimeline);
    }
  };
  using Timeline = UniquePtr<ffi::WGPUVulkanTimeline, TimelineDeleter>;

  struct ReturnState {
    Mutex mMutex{"SharedTextureVulkan::ReturnState"};
    Maybe<layers::VulkanImageReturnMessage> mResult MOZ_GUARDED_BY(mMutex);
  };

  const ffi::WGPUDeviceId mDeviceId;
  ffi::WGPUVulkanDmaBufInfo mInfo{};
  RefPtr<gfx::FileHandleWrapper> mMemory;
  layers::VulkanTimelineDescriptor mReadyDescriptor;
  Timeline mReady;
  Timeline mReturned;
  uint64_t mAcquireValue = 0;
  Maybe<layers::VulkanImagePublication> mPublication;
  std::shared_ptr<ReturnState> mReturn;
};

}  // namespace mozilla::webgpu

#endif
