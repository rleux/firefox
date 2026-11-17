/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
#include "RenderVulkanSharedTextureHost.h"

#include "mozilla/CheckedInt.h"
#include "mozilla/webrender/RenderThread.h"
#include "nsXULAppAPI.h"

namespace mozilla::wr {
already_AddRefed<RenderVulkanSharedTextureHost>
RenderVulkanSharedTextureHost::Create(std::shared_ptr<void> aLease,
                                      uint64_t aOwner, uint64_t aPublication,
                                      const gfx::IntSize& aSize,
                                      gfx::SurfaceFormat aFormat) {
  if (!XRE_IsGPUProcess() || !aLease || !aOwner || !aPublication ||
      aSize.width <= 0 || aSize.height <= 0 ||
      !(CheckedInt<size_t>(aSize.width) * aSize.height * 4).isValid() ||
      (aFormat != gfx::SurfaceFormat::R8G8B8A8 &&
       aFormat != gfx::SurfaceFormat::B8G8R8A8)) {
    return nullptr;
  }
  RefPtr<RenderVulkanSharedTextureHost> host =
      new RenderVulkanSharedTextureHost(std::move(aLease), aOwner, aPublication,
                                        aSize, aFormat);
  return host.forget();
}
RenderVulkanSharedTextureHost::RenderVulkanSharedTextureHost(
    std::shared_ptr<void> aLease, uint64_t aOwner, uint64_t aPublication,
    const gfx::IntSize& aSize, gfx::SurfaceFormat aFormat)
    : mLease(std::move(aLease)),
      mOwner(aOwner),
      mPublication(aPublication),
      mSize(aSize),
      mFormat(aFormat) {}
RenderVulkanSharedTextureHost::~RenderVulkanSharedTextureHost() {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
}
WrExternalImage RenderVulkanSharedTextureHost::LockVulkan(
    uint8_t aChannel, WrVulkanExternalImages* aImages) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (!aImages || aChannel > 1 ||
      !wr_vulkan_shared_image_supported(aImages, mLease.get(), mOwner,
                                        mPublication)) {
    return InvalidToWrExternalImage();
  }
  if (mImage &&
      !wr_vulkan_shared_image_matches_context(mImage.get(), aImages)) {
    mImage.reset();
  }
  if (!mImage) {
    mImage.reset(wr_vulkan_shared_image_new(aImages, mLease.get(), mOwner,
                                            mPublication));
  }
  if (!mImage) {
    return InvalidToWrExternalImage();
  }
  ExternalTextureHandle handle{};
  const auto status =
      wr_vulkan_shared_image_acquire(mImage.get(), aChannel, &handle);
  if (status == WrVulkanAcquireStatus::Pending) {
    return PendingToWrExternalImage();
  }
  if (status != WrVulkanAcquireStatus::Ready) {
    return InvalidToWrExternalImage();
  }
  return NativeTextureToWrExternalImage(handle._0, 0, 0, float(mSize.width),
                                        float(mSize.height));
}
Maybe<VulkanImageRelease> RenderVulkanSharedTextureHost::UnlockVulkan(
    WrVulkanExternalImages*) {
  return Nothing();
}
bool RenderVulkanSharedTextureHost::CanReadbackVulkan(
    WrVulkanExternalImages* aImages, bool aAllowMigration) const {
  return aImages &&
         (aAllowMigration || (mImage && wr_vulkan_shared_image_matches_context(
                                            mImage.get(), aImages))) &&
         wr_vulkan_shared_image_supported(aImages, mLease.get(), mOwner,
                                          mPublication);
}
bool RenderVulkanSharedTextureHost::ReadbackVulkan(uint8_t* aOutput,
                                                   size_t aLength) {
  return mImage &&
         wr_vulkan_shared_image_readback(mImage.get(), aOutput, aLength);
}
}  // namespace mozilla::wr
