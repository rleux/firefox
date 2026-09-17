/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderDMABUFTextureHost.h"

#include <algorithm>

#include "GLContextEGL.h"
#include "ScopedGLHelpers.h"
#include "mozilla/gfx/FileHandleWrapper.h"
#include "mozilla/gfx/Logging.h"

namespace mozilla::wr {

RenderDMABUFTextureHost::RenderDMABUFTextureHost(DMABufSurface* aSurface)
    : mSurface(aSurface) {
  MOZ_COUNT_CTOR_INHERITED(RenderDMABUFTextureHost, RenderTextureHost);
}

RenderDMABUFTextureHost::~RenderDMABUFTextureHost() {
  MOZ_COUNT_DTOR_INHERITED(RenderDMABUFTextureHost, RenderTextureHost);
  DeleteTextureHandle();
}

wr::WrExternalImage RenderDMABUFTextureHost::Lock(uint8_t aChannelIndex,
                                                  gl::GLContext* aGL) {
  if (auto* yuv = mSurface->GetAsDMABufSurfaceYUV();
      yuv && yuv->GetVAAPIDescriptor()) {
    return InvalidToWrExternalImage();
  }
  const gfx::IntSize size(mSurface->GetWidth(aChannelIndex),
                          mSurface->GetHeight(aChannelIndex));

  // Wayland native compositor doesn't use textures so pass zero
  // there. It saves GPU resources.
  if (!aGL) {
    return NativeTextureToWrExternalImage(0, 0.0, 0.0,
                                          static_cast<float>(size.width),
                                          static_cast<float>(size.height));
  }

  if (mGL.get() != aGL) {
    if (mGL) {
      // This should not happen. EGLImage is created only in
      // parent process.
      MOZ_ASSERT_UNREACHABLE("Unexpected GL context");
      return InvalidToWrExternalImage();
    }
    mGL = aGL;
  }

  if (!mGL || !mGL->MakeCurrent()) {
    return InvalidToWrExternalImage();
  }

  if (!mSurface->GetTexture(aChannelIndex)) {
    if (!mSurface->CreateTexture(mGL, aChannelIndex)) {
      return InvalidToWrExternalImage();
    }
    ActivateBindAndTexParameteri(mGL, LOCAL_GL_TEXTURE0, LOCAL_GL_TEXTURE_2D,
                                 mSurface->GetTexture(aChannelIndex));
  }

  if (auto texture = mSurface->GetTexture(aChannelIndex)) {
    mSurface->MaybeSemaphoreWait(texture);
  }

  return NativeTextureToWrExternalImage(
      mSurface->GetTexture(aChannelIndex), 0.0, 0.0,
      static_cast<float>(size.width), static_cast<float>(size.height));
}

gfx::IntSize RenderDMABUFTextureHost::GetSize(uint8_t aChannelIndex) const {
  MOZ_ASSERT(mSurface);
  MOZ_ASSERT((mSurface->GetTextureCount() == 0)
                 ? (aChannelIndex == mSurface->GetTextureCount())
                 : (aChannelIndex < mSurface->GetTextureCount()));

  if (!mSurface) {
    return gfx::IntSize();
  }
  return gfx::IntSize(mSurface->GetWidth(aChannelIndex),
                      mSurface->GetHeight(aChannelIndex));
}

void RenderDMABUFTextureHost::Unlock() {}

bool RenderDMABUFTextureHost::GetVAAPIImage(const DMABufSurfaceYUV& aSurface,
                                            uint8_t aChannelIndex,
                                            WrHalImage* aImage) {
  const auto* desc = aSurface.GetVAAPIDescriptor();
  if (!desc || aChannelIndex > 1 || !aSurface.AccessLockUsable()) {
    return false;
  }
  const auto& state = desc->vaapiImageState().ref();
  if (state.objects().Length() != 1) {
    return false;
  }
  const auto& object = state.objects()[0];
  WrHalNv12 image{};
  image.fd = object.fd()->GetHandle();
  image.access_lock_fd = state.accessLock()->GetHandle();
  image.width = desc->width()[0];
  image.height = desc->height()[0];
  image.allocation_width = desc->widthAligned()[0];
  image.allocation_height = desc->heightAligned()[0];
  image.allocation_size = object.size();
  image.modifier = object.modifier();
  for (size_t i = 0; i < 2; ++i) {
    image.strides[i] = state.planes()[i].stride();
    image.offsets[i] = state.planes()[i].offset();
  }
  image.allocation_id = state.allocationId();
  image.producer_epoch = state.producerEpoch();
  image.drm_node[0] = state.drmRenderMajor();
  image.drm_node[1] = state.drmRenderMinor();
  *aImage = WrHalImage{state.generation(), WrHalImageSource::Nv12(image)};
  return true;
}

bool RenderDMABUFTextureHost::LockHalImage(uint8_t aChannelIndex,
                                           WrHalImage* aImage) {
  if (mVulkanFailed) {
    return false;
  }
  if (auto* yuv = mSurface->GetAsDMABufSurfaceYUV()) {
    return GetVAAPIImage(*yuv, aChannelIndex, aImage);
  }
  if (aChannelIndex) {
    return false;
  }
  if (const auto* foreign = mSurface->GetForeignRGBDescriptor()) {
    WrHalForeignRGB image{};
    image.fd = foreign->fds()[0]->GetHandle();
    image.ready_fd = foreign->fence()[0]->GetHandle();
    image.width = foreign->width()[0];
    image.height = foreign->height()[0];
    image.fourcc = foreign->fourccFormat();
    image.stride = foreign->strides()[0];
    image.offset = foreign->offsets()[0];
    *aImage = WrHalImage{foreign->foreignRGBImageState()->generation(),
                         WrHalImageSource::ForeignRGB(image)};
    return true;
  }
  const auto* desc = mSurface->GetVulkanDescriptor();
  if (mVulkanFailed || aChannelIndex || !desc || !desc->vulkanImageState() ||
      desc->fds().Length() != 1 || !desc->semaphoreFdIsSyncFd()) {
    return false;
  }
  const auto& state = desc->vulkanImageState().ref();
  ImageFormat format;
  switch (mSurface->GetFormat()) {
    case gfx::SurfaceFormat::B8G8R8A8:
      format = ImageFormat::BGRA8;
      break;
    case gfx::SurfaceFormat::R8G8B8A8:
      format = ImageFormat::RGBA8;
      break;
    default:
      return false;
  }
  if (mVulkanLocked || !mSurface->LockAccess()) {
    return false;
  }
  // The HAL lease returns external ownership before unlocking this surface.
  mVulkanLocked = true;
  WrHalDmaBuf image{};
  image.fd = desc->fds()[0]->GetHandle();
  image.ready_fd = desc->semaphoreFd() ? desc->semaphoreFd()->GetHandle() : -1;
  image.width = desc->width()[0];
  image.height = desc->height()[0];
  image.format = format;
  image.modifier = desc->modifier()[0];
  image.stride = desc->strides()[0];
  image.offset = desc->offsets()[0];
  std::copy_n(state.deviceUUID().Elements(), 16, image.device_uuid);
  std::copy_n(state.driverUUID().Elements(), 16, image.driver_uuid);
  *aImage =
      WrHalImage{state.generation(), WrHalImageSource::VulkanDmaBuf(image)};
  return true;
}

void RenderDMABUFTextureHost::UnlockHalImage(WrHalImageRelease aStatus) {
  if (mVulkanLocked) {
    mSurface->UnlockAccess(aStatus == WrHalImageRelease::Abandoned);
    mVulkanLocked = false;
  }
  if (aStatus == WrHalImageRelease::Abandoned) {
    auto* yuv = mSurface->GetAsDMABufSurfaceYUV();
    if (!mVulkanFailed &&
        (mSurface->IsForeignRGB() || (yuv && yuv->GetVAAPIDescriptor()))) {
      mSurface->GlobalRefAdd();
    }
    mVulkanFailed = true;
  }
}

void RenderDMABUFTextureHost::DeleteTextureHandle() {
  mSurface->ReleaseTextures();
}

void RenderDMABUFTextureHost::ClearCachedResources() {
  DeleteTextureHandle();
  mGL = nullptr;
}

bool RenderDMABUFTextureHost::MapPlane(RenderCompositor* aCompositor,
                                       uint8_t aChannelIndex,
                                       PlaneInfo& aPlaneInfo) {
  if (mSurface->GetAsDMABufSurfaceYUV()) {
    // DMABufSurfaceYUV is not supported.
    return false;
  }

  const RefPtr<gfx::SourceSurface> surface = mSurface->GetAsSourceSurface();
  if (!surface) {
    return false;
  }

  const RefPtr<gfx::DataSourceSurface> dataSurface = surface->GetDataSurface();
  if (!dataSurface) {
    return false;
  }

  gfx::DataSourceSurface::MappedSurface map;
  if (!dataSurface->Map(gfx::DataSourceSurface::MapType::READ, &map)) {
    return false;
  }

  mReadback = dataSurface;
  aPlaneInfo.mSize = gfx::IntSize(mSurface->GetWidth(), mSurface->GetHeight());
  aPlaneInfo.mStride = map.mStride;
  aPlaneInfo.mData = map.mData;

  return true;
}

void RenderDMABUFTextureHost::UnmapPlanes() {
  if (mReadback) {
    mReadback->Unmap();
    mReadback = nullptr;
  }
}

}  // namespace mozilla::wr
