/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
#ifndef MOZILLA_GFX_RENDERVULKANSHAREDTEXTUREHOST_H
#define MOZILLA_GFX_RENDERVULKANSHAREDTEXTUREHOST_H

#include <memory>

#include "RenderTextureHost.h"

namespace mozilla::wr {
class RenderVulkanSharedTextureHost final : public RenderTextureHost {
 public:
  static already_AddRefed<RenderVulkanSharedTextureHost> Create(
      std::shared_ptr<void>, uint64_t aOwner, uint64_t aPublication,
      const gfx::IntSize&, gfx::SurfaceFormat);
  WrExternalImage LockVulkan(uint8_t, WrVulkanExternalImages*) override;
  Maybe<VulkanImageRelease> UnlockVulkan(WrVulkanExternalImages*) override;
  uint64_t VulkanPublicationId() const override { return mPublication; }
  uint64_t VulkanOwnerId() const override { return mOwner; }
  bool CanReadbackVulkan(WrVulkanExternalImages*, bool) const override;
  bool ReadbackVulkan(uint8_t*, size_t) override;
  size_t Bytes() override { return size_t(mSize.width) * mSize.height * 4; }
  gfx::SurfaceFormat GetFormat() const override { return mFormat; }

 private:
  RenderVulkanSharedTextureHost(std::shared_ptr<void>, uint64_t, uint64_t,
                                const gfx::IntSize&, gfx::SurfaceFormat);
  ~RenderVulkanSharedTextureHost() override;
  struct Deleter {
    void operator()(WrVulkanSharedImage* aImage) {
      wr_vulkan_shared_image_delete(aImage);
    }
  };
  std::shared_ptr<void> mLease;
  UniquePtr<WrVulkanSharedImage, Deleter> mImage;
  const uint64_t mOwner;
  const uint64_t mPublication;
  const gfx::IntSize mSize;
  const gfx::SurfaceFormat mFormat;
};
}  // namespace mozilla::wr
#endif
