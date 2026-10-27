/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderVulkanNativeTextureHost.h"

#include <limits>

#include "RenderThread.h"
#include "mozilla/CheckedInt.h"

namespace mozilla::wr {

already_AddRefed<RenderVulkanNativeTextureHost>
RenderVulkanNativeTextureHost::Create(WrVulkanNativePublication* aPublication) {
  Publication publication(aPublication);
  WrVulkanNativeImageInfo info{};
  if (!publication ||
      !wr_vulkan_native_publication_info(publication.get(), &info) ||
      !info.width || !info.height ||
      info.width > uint32_t(std::numeric_limits<int32_t>::max()) ||
      info.height > uint32_t(std::numeric_limits<int32_t>::max())) {
    return nullptr;
  }
  gfx::SurfaceFormat format;
  switch (info.format) {
    case ImageFormat::RGBA8:
      format = gfx::SurfaceFormat::R8G8B8A8;
      break;
    case ImageFormat::BGRA8:
      format = gfx::SurfaceFormat::B8G8R8A8;
      break;
    default:
      return nullptr;
  }
  const CheckedInt<size_t> bytes =
      CheckedInt<size_t>(info.width) * info.height * 4;
  if (!bytes.isValid()) {
    return nullptr;
  }
  RefPtr<RenderVulkanNativeTextureHost> host =
      new RenderVulkanNativeTextureHost(
          std::move(publication),
          gfx::IntSize(int32_t(info.width), int32_t(info.height)), format,
          bytes.value());
  return host.forget();
}

RenderVulkanNativeTextureHost::RenderVulkanNativeTextureHost(
    Publication&& aPublication, const gfx::IntSize& aSize,
    gfx::SurfaceFormat aFormat, size_t aBytes)
    : mPublication(std::move(aPublication)),
      mSize(aSize),
      mFormat(aFormat),
      mBytes(aBytes) {
  MOZ_COUNT_CTOR_INHERITED(RenderVulkanNativeTextureHost, RenderTextureHost);
}

RenderVulkanNativeTextureHost::~RenderVulkanNativeTextureHost() {
  MOZ_ASSERT(!mImage || RenderThread::IsInRenderThread());
  MOZ_COUNT_DTOR_INHERITED(RenderVulkanNativeTextureHost, RenderTextureHost);
}

WrExternalImage RenderVulkanNativeTextureHost::LockVulkan(
    uint8_t aChannelIndex, WrVulkanExternalImages* aImages) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (aChannelIndex > 1 || !aImages) {
    return InvalidToWrExternalImage();
  }
  if (!mImage ||
      !wr_vulkan_native_image_matches_context(mImage.get(), aImages)) {
    mozilla::UniquePtr<WrVulkanNativeImage, ImageDeleter> replacement(
        wr_vulkan_native_image_new(aImages, mPublication.get()));
    if (!replacement) {
      return InvalidToWrExternalImage();
    }
    mImage = std::move(replacement);
  }
  ExternalTextureHandle handle{};
  if (!mImage || !wr_vulkan_native_image_lock(mImage.get(), aImages,
                                              aChannelIndex, &handle)) {
    return InvalidToWrExternalImage();
  }
  return NativeTextureToWrExternalImage(handle._0, 0, 0, float(mSize.width),
                                        float(mSize.height));
}

void RenderVulkanNativeTextureHost::ClearCachedResources() {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  mImage.reset();
}

bool RenderVulkanNativeTextureHost::Snapshot(const DeviceIntRect& aRect,
                                             size_t aStride,
                                             uint8_t* aDestination,
                                             size_t aLength,
                                             ImageFormat* aFormat) const {
  return wr_vulkan_native_publication_snapshot(
      mPublication.get(), aRect, aStride, aDestination, aLength, aFormat);
}

}  // namespace mozilla::wr
