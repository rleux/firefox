/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_RENDERVULKANNATIVETEXTUREHOST_H
#define MOZILLA_GFX_RENDERVULKANNATIVETEXTUREHOST_H

#include "RenderTextureHost.h"
#include "mozilla/UniquePtr.h"

namespace mozilla::wr {

class RenderVulkanNativeTextureHost final : public RenderTextureHost {
 public:
  // Takes unique ownership, including when metadata validation fails.
  static already_AddRefed<RenderVulkanNativeTextureHost> Create(
      WrVulkanNativePublication* aPublication);

  WrExternalImage LockVulkan(uint8_t aChannelIndex,
                             WrVulkanExternalImages* aImages) override;
  Maybe<VulkanImageRelease> UnlockVulkan(WrVulkanExternalImages*) override {
    return Nothing();
  }
  void ClearCachedResources() override;
  size_t Bytes() override { return mBytes; }
  gfx::SurfaceFormat GetFormat() const override { return mFormat; }
  const gfx::IntSize& GetSize() const { return mSize; }

  bool Snapshot(const DeviceIntRect& aRect, size_t aStride,
                uint8_t* aDestination, size_t aLength,
                ImageFormat* aFormat) const;

 private:
  struct PublicationDeleter {
    void operator()(WrVulkanNativePublication* aPublication) {
      wr_vulkan_native_publication_delete(aPublication);
    }
  };
  struct ImageDeleter {
    void operator()(WrVulkanNativeImage* aImage) {
      wr_vulkan_native_image_delete(aImage);
    }
  };
  using Publication =
      mozilla::UniquePtr<WrVulkanNativePublication, PublicationDeleter>;

  RenderVulkanNativeTextureHost(Publication&&, const gfx::IntSize&,
                                gfx::SurfaceFormat, size_t);
  ~RenderVulkanNativeTextureHost() override;

  Publication mPublication;
  mozilla::UniquePtr<WrVulkanNativeImage, ImageDeleter> mImage;
  const gfx::IntSize mSize;
  const gfx::SurfaceFormat mFormat;
  const size_t mBytes;
};

}  // namespace mozilla::wr

#endif
