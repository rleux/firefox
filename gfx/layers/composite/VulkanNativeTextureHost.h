/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_VULKANNATIVETEXTUREHOST_H
#define MOZILLA_GFX_VULKANNATIVETEXTUREHOST_H

#include "mozilla/UniquePtr.h"
#include "mozilla/layers/TextureHost.h"
#include "mozilla/webgpu/FrontBufferSnapshot.h"

namespace mozilla::layers {

class VulkanNativeTextureHost final : public TextureHost {
 public:
  // Takes unique ownership, including failed creation.
  static already_AddRefed<VulkanNativeTextureHost> Create(
      TextureFlags aFlags, wr::WrVulkanNativePublication* aPublication);

  gfx::SurfaceFormat GetFormat() const override { return mFormat; }
  gfx::IntSize GetSize() const override { return mSize; }
  VulkanNativeTextureHost* AsVulkanNativeTextureHost() override { return this; }
  already_AddRefed<gfx::DataSourceSurface> GetAsSurface(
      gfx::DataSourceSurface*) override {
    return nullptr;
  }

  webgpu::FrontBufferSnapshot GetSnapshot() const;

  void PushResourceUpdates(wr::TransactionBuilder&, ResourceUpdateOp,
                           const Range<wr::ImageKey>&,
                           const wr::ExternalImageId&) override;
  void PushDisplayItems(wr::DisplayListBuilder&, const wr::LayoutRect& aBounds,
                        const wr::LayoutRect& aClip, wr::ImageRendering,
                        const Range<wr::ImageKey>&,
                        PushDisplayItemFlagSet) override;

 private:
  struct PublicationDeleter {
    void operator()(wr::WrVulkanNativePublication* aPublication) {
      wr::wr_vulkan_native_publication_delete(aPublication);
    }
  };
  using Publication =
      mozilla::UniquePtr<wr::WrVulkanNativePublication, PublicationDeleter>;
  VulkanNativeTextureHost(TextureFlags, Publication&&, const gfx::IntSize&,
                          gfx::SurfaceFormat);
  ~VulkanNativeTextureHost() override = default;

  Publication mPublication;
  const gfx::IntSize mSize;
  const gfx::SurfaceFormat mFormat;
};

}  // namespace mozilla::layers

#endif
