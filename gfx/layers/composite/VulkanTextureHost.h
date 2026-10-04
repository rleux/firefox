/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_VULKANTEXTUREHOST_H
#define MOZILLA_GFX_VULKANTEXTUREHOST_H

#include <functional>

#include "mozilla/layers/TextureHost.h"
#include "mozilla/layers/VulkanImages.h"

namespace mozilla::layers {

class VulkanTextureHost final : public TextureHost {
 public:
  // Registers one immutable publication. The callback runs on the render
  // thread; rejection leaves ownership with the caller and does not call it.
  static already_AddRefed<VulkanTextureHost> Create(
      TextureFlags aFlags, const VulkanImagePublication& aPublication,
      std::function<void(VulkanImageReturnMessage&&)>&& aReturn);

  gfx::SurfaceFormat GetFormat() const override { return mFormat; }
  gfx::IntSize GetSize() const override { return mSize; }
  already_AddRefed<gfx::DataSourceSurface> GetAsSurface(
      gfx::DataSourceSurface*) override {
    return nullptr;
  }

  void PushResourceUpdates(wr::TransactionBuilder&, ResourceUpdateOp,
                           const Range<wr::ImageKey>&,
                           const wr::ExternalImageId&) override;
  void PushDisplayItems(wr::DisplayListBuilder&, const wr::LayoutRect& aBounds,
                        const wr::LayoutRect& aClip, wr::ImageRendering,
                        const Range<wr::ImageKey>&,
                        PushDisplayItemFlagSet) override;

 private:
  VulkanTextureHost(TextureFlags, const VulkanImagePublication&);
  ~VulkanTextureHost() override = default;
  const gfx::IntSize mSize;
  const gfx::SurfaceFormat mFormat;
};

}  // namespace mozilla::layers

#endif
