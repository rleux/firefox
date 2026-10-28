/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanNativeTextureHost.h"

#include <cstring>

#include "mozilla/CheckedInt.h"
#include "mozilla/ipc/SharedMemoryMapping.h"
#include "mozilla/layers/AsyncImagePipelineManager.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/webrender/RenderVulkanNativeTextureHost.h"
#include "mozilla/webrender/WebRenderAPI.h"

namespace mozilla::layers {

already_AddRefed<VulkanNativeTextureHost> VulkanNativeTextureHost::Create(
    TextureFlags aFlags, wr::WrVulkanNativePublication* aPublication) {
  Publication publication(aPublication);
  auto* thread = wr::RenderThread::Get();
  if (!publication || !thread) {
    return nullptr;
  }
  RefPtr<wr::RenderVulkanNativeTextureHost> render =
      wr::RenderVulkanNativeTextureHost::Create(
          wr::wr_vulkan_native_publication_clone(publication.get()));
  if (!render) {
    return nullptr;
  }
  RefPtr<VulkanNativeTextureHost> host = new VulkanNativeTextureHost(
      aFlags, std::move(publication), render->GetSize(), render->GetFormat());
  host->mExternalImageId =
      Some(AsyncImagePipelineManager::GetNextExternalImageId());
  thread->RegisterExternalImage(host->mExternalImageId.ref(), render.forget());
  return host.forget();
}

already_AddRefed<VulkanNativeTextureHost> VulkanNativeTextureHost::CreateError(
    TextureFlags aFlags, const gfx::IntSize& aSize,
    gfx::SurfaceFormat aFormat) {
  if (aSize.width < 0 || aSize.height < 0 ||
      (aFormat != gfx::SurfaceFormat::R8G8B8A8 &&
       aFormat != gfx::SurfaceFormat::B8G8R8A8)) {
    return nullptr;
  }
  RefPtr<VulkanNativeTextureHost> host =
      new VulkanNativeTextureHost(aFlags, Publication(), aSize, aFormat, true);
  return host.forget();
}

VulkanNativeTextureHost::VulkanNativeTextureHost(TextureFlags aFlags,
                                                 Publication&& aPublication,
                                                 const gfx::IntSize& aSize,
                                                 gfx::SurfaceFormat aFormat,
                                                 bool aIsError)
    : TextureHost(TextureHostType::VulkanNative, aFlags),
      mPublication(std::move(aPublication)),
      mSize(aSize),
      mFormat(aFormat),
      mOpaque(!!(aFlags & TextureFlags::IS_OPAQUE)),
      mIsError(aIsError) {}

webgpu::FrontBufferSnapshot VulkanNativeTextureHost::GetSnapshot() const {
  if ((!mPublication && !mIsError) || mSize.width <= 0 || mSize.height <= 0) {
    return {};
  }
  const CheckedInt<size_t> stride = CheckedInt<size_t>(mSize.width) * 4;
  const auto length = stride * mSize.height;
  if (!length.isValid()) {
    return {};
  }
  auto handle = ipc::shared_memory::Create(length.value());
  auto mapping = handle.Map();
  if (!handle || !mapping) {
    return {};
  }
  if (mIsError) {
    memset(mapping.DataAs<uint8_t>(), 0, length.value());
    return {std::move(handle), uint32_t(mSize.width), uint32_t(mSize.height),
            stride.value()};
  }
  wr::DeviceIntRect rect;
  rect.min.x = 0;
  rect.min.y = 0;
  rect.max.x = mSize.width;
  rect.max.y = mSize.height;
  auto format = wr::ImageFormat::R8;
  if (!wr::wr_vulkan_native_publication_snapshot(
          mPublication.get(), rect, stride.value(), mapping.DataAs<uint8_t>(),
          mapping.Size(), &format)) {
    return {};
  }
  const auto expected = wr::SurfaceFormatToImageFormat(mFormat);
  if (!expected || format != *expected) {
    return {};
  }
  return {std::move(handle), uint32_t(mSize.width), uint32_t(mSize.height),
          stride.value()};
}

void VulkanNativeTextureHost::PushResourceUpdates(
    wr::TransactionBuilder& aResources, ResourceUpdateOp aOp,
    const Range<wr::ImageKey>& aImageKeys, const wr::ExternalImageId& aExtID) {
  MOZ_RELEASE_ASSERT(!mIsError && aImageKeys.length() == 1);
  const auto format = wr::SurfaceFormatToImageFormat(mFormat);
  MOZ_RELEASE_ASSERT(format);
  wr::ImageDescriptor descriptor(
      mSize, *format,
      mOpaque ? wr::OpacityType::Opaque : wr::OpacityType::HasAlphaChannel);
  const auto type =
      wr::ExternalImageType::TextureHandle(wr::ImageBufferKind::Texture2D);
  const auto method = aOp == ADD_IMAGE
                          ? &wr::TransactionBuilder::AddExternalImage
                          : &wr::TransactionBuilder::UpdateExternalImage;
  const uint8_t channel = mOpaque ? 1 : 0;
  (aResources.*method)(aImageKeys[0], descriptor, aExtID, type, channel, false);
}

void VulkanNativeTextureHost::PushDisplayItems(
    wr::DisplayListBuilder& aBuilder, const wr::LayoutRect& aBounds,
    const wr::LayoutRect& aClip, wr::ImageRendering aFilter,
    const Range<wr::ImageKey>& aImageKeys, PushDisplayItemFlagSet) {
  if (mIsError) {
    if (mOpaque) {
      aBuilder.PushRect(aBounds, aClip, true, false, false,
                        wr::ColorF{0.0f, 0.0f, 0.0f, 1.0f});
    }
    return;
  }
  MOZ_RELEASE_ASSERT(aImageKeys.length() == 1);
  aBuilder.PushImage(aBounds, aClip, true, false, aFilter, aImageKeys[0], true,
                     wr::ColorF{1.0f, 1.0f, 1.0f, 1.0f});
}

}  // namespace mozilla::layers
