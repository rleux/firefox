/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanTextureHost.h"

#include "mozilla/layers/AsyncImagePipelineManager.h"
#include "mozilla/webrender/RenderTextureHost.h"
#if defined(MOZ_WIDGET_GTK)
#  include "mozilla/webrender/RenderVulkanSharedTextureHost.h"
#endif
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/webrender/VulkanImageIPC.h"
#include "mozilla/webrender/WebRenderAPI.h"

namespace mozilla::layers {

already_AddRefed<VulkanTextureHost> VulkanTextureHost::Create(
    TextureFlags aFlags, const VulkanImagePublication& aPublication,
    std::function<void(VulkanImageReturnMessage&&)>&& aReturn) {
  RefPtr<wr::RenderTextureHost> texture =
      wr::CreateVulkanImageHost(aPublication, std::move(aReturn));
  if (!texture) {
    return nullptr;
  }
  RefPtr<VulkanTextureHost> host = new VulkanTextureHost(aFlags, aPublication);
  host->mExternalImageId =
      Some(AsyncImagePipelineManager::GetNextExternalImageId());
  wr::RenderThread::Get()->RegisterExternalImage(host->mExternalImageId.ref(),
                                                 texture.forget());
  return host.forget();
}

already_AddRefed<VulkanTextureHost> VulkanTextureHost::CreateShared(
    TextureFlags aFlags, std::shared_ptr<void> aLease, uint64_t aOwner,
    uint64_t aPublication, const gfx::IntSize& aSize,
    gfx::SurfaceFormat aFormat) {
#if defined(MOZ_WIDGET_GTK)
  if (!wr::RenderThread::Get()) {
    return nullptr;
  }
  RefPtr<wr::RenderTextureHost> texture =
      wr::RenderVulkanSharedTextureHost::Create(std::move(aLease), aOwner,
                                                aPublication, aSize, aFormat);
  if (!texture) {
    return nullptr;
  }
  RefPtr<VulkanTextureHost> host =
      new VulkanTextureHost(aFlags, aSize, aFormat);
  host->mExternalImageId =
      Some(AsyncImagePipelineManager::GetNextExternalImageId());
  wr::RenderThread::Get()->RegisterExternalImage(host->mExternalImageId.ref(),
                                                 texture.forget());
  return host.forget();
#else
  return nullptr;
#endif
}

VulkanTextureHost::VulkanTextureHost(TextureFlags aFlags,
                                     const gfx::IntSize& aSize,
                                     gfx::SurfaceFormat aFormat)
    : TextureHost(TextureHostType::Vulkan, aFlags),
      mSize(aSize),
      mFormat(aFormat) {}

VulkanTextureHost::VulkanTextureHost(TextureFlags aFlags,
                                     const VulkanImagePublication& aPublication)
    : TextureHost(TextureHostType::Vulkan, aFlags),
      mSize(aPublication.size()),
      mFormat(aPublication.format()) {}

webgpu::FrontBufferSnapshot VulkanTextureHost::GetSnapshot() {
  RefPtr<wr::RenderThread> thread = wr::RenderThread::Get();
  if (!mExternalImageId || !thread) {
    return {};
  }
  return thread->SnapshotNativeImage(
      mExternalImageId.ref(), mSize,
      bool(GetFlags() & TextureFlags::IS_OPAQUE));
}

void VulkanTextureHost::PushResourceUpdates(
    wr::TransactionBuilder& aResources, ResourceUpdateOp aOp,
    const Range<wr::ImageKey>& aImageKeys, const wr::ExternalImageId& aExtID) {
  MOZ_RELEASE_ASSERT(aImageKeys.length() == 1);
  const auto format = wr::SurfaceFormatToImageFormat(mFormat);
  MOZ_RELEASE_ASSERT(format);
  wr::ImageDescriptor descriptor(mSize, *format,
                                 (GetFlags() & TextureFlags::IS_OPAQUE)
                                     ? wr::OpacityType::Opaque
                                     : wr::OpacityType::HasAlphaChannel);
  const auto type =
      wr::ExternalImageType::TextureHandle(wr::ImageBufferKind::Texture2D);
  const auto method = aOp == ADD_IMAGE
                          ? &wr::TransactionBuilder::AddExternalImage
                          : &wr::TransactionBuilder::UpdateExternalImage;
  const uint8_t channel = (GetFlags() & TextureFlags::IS_OPAQUE) ? 1 : 0;
  (aResources.*method)(aImageKeys[0], descriptor, aExtID, type, channel,
                       /* aNormalizedUvs */ false);
}

void VulkanTextureHost::PushDisplayItems(wr::DisplayListBuilder& aBuilder,
                                         const wr::LayoutRect& aBounds,
                                         const wr::LayoutRect& aClip,
                                         wr::ImageRendering aFilter,
                                         const Range<wr::ImageKey>& aImageKeys,
                                         PushDisplayItemFlagSet aFlags) {
  MOZ_RELEASE_ASSERT(aImageKeys.length() == 1);
  aBuilder.PushImage(aBounds, aClip, true, false, aFilter, aImageKeys[0],
                     !(mFlags & TextureFlags::NON_PREMULTIPLIED),
                     wr::ColorF{1.0f, 1.0f, 1.0f, 1.0f});
}

}  // namespace mozilla::layers
