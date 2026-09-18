/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "SharedTextureDMABuf.h"

#include <algorithm>

#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/Logging.h"
#include "mozilla/webgpu/WebGPUParent.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "mozilla/widget/DMABufDevice.h"
#include "mozilla/widget/DMABufSurface.h"

namespace mozilla::webgpu {

// static
UniquePtr<SharedTextureDMABuf> SharedTextureDMABuf::Create(
    WebGPUParent* aParent, const ffi::WGPUDeviceId aDeviceId,
    const uint32_t aWidth, const uint32_t aHeight,
    const struct ffi::WGPUTextureFormat aFormat,
    const ffi::WGPUTextureUsages aUsage) {
  const bool forWebRender = wr::RenderCompositorVulkan::IsRequested();
  if (aFormat.tag != ffi::WGPUTextureFormat_Bgra8Unorm &&
      !(forWebRender && aFormat.tag == ffi::WGPUTextureFormat_Rgba8Unorm)) {
    gfxCriticalNoteOnce << "Non supported format: " << aFormat.tag;
    return nullptr;
  }

  auto* context = aParent->GetContext();
  int32_t rawFd = -1;
  ffi::WGPUDMABufInfo dmaBufInfo = ffi::wgpu_vkimage_create_with_dma_buf(
      context, aDeviceId, aWidth, aHeight, aFormat, aUsage, forWebRender,
      &rawFd);
  if (!dmaBufInfo.is_valid || rawFd < 0) {
    gfxCriticalNoteOnce << "Failed to create dma-buf backed VkImage";
    return nullptr;
  }

  RefPtr<gfx::FileHandleWrapper> fd =
      new gfx::FileHandleWrapper(UniqueFileHandle(rawFd));

  MOZ_ASSERT(dmaBufInfo.plane_count <= 3);

  if (dmaBufInfo.plane_count > 3) {
    gfxCriticalNoteOnce << "Invalid plane count";
    return nullptr;
  }

  RefPtr<DMABufSurface> surface = DMABufSurfaceRGBA::CreateDMABufSurface(
      std::move(fd), dmaBufInfo, aWidth, aHeight);
  if (!surface) {
    MOZ_ASSERT_UNREACHABLE("unexpected to be called");
    return nullptr;
  }
  if (forWebRender && !surface->CreateAccessLock()) {
    return nullptr;
  }

  layers::SurfaceDescriptor desc;
  if (!surface->Serialize(desc)) {
    MOZ_ASSERT_UNREACHABLE("unexpected to be called");
    return nullptr;
  }

  const auto sdType = desc.type();
  if (sdType != layers::SurfaceDescriptor::TSurfaceDescriptorDMABuf) {
    MOZ_ASSERT_UNREACHABLE("unexpected to be called");
    return nullptr;
  }

  return MakeUnique<SharedTextureDMABuf>(
      aWidth, aHeight, aFormat, aUsage, std::move(surface),
      desc.get_SurfaceDescriptorDMABuf(), dmaBufInfo);
}

SharedTextureDMABuf::SharedTextureDMABuf(
    const uint32_t aWidth, const uint32_t aHeight,
    const struct ffi::WGPUTextureFormat aFormat,
    const ffi::WGPUTextureUsages aUsage, RefPtr<DMABufSurface>&& aSurface,
    const layers::SurfaceDescriptorDMABuf& aSurfaceDescriptor,
    const ffi::WGPUDMABufInfo& aDMABufInfo)
    : SharedTexture(aWidth, aHeight, aFormat, aUsage),
      mSurface(std::move(aSurface)),
      mSurfaceDescriptor(aSurfaceDescriptor),
      mDMABufInfo(aDMABufInfo) {}

SharedTextureDMABuf::~SharedTextureDMABuf() = default;

bool SharedTextureDMABuf::CanRetryVulkanRetirement() const {
  return mDMABufInfo.for_webrender && mSurface->AccessLockUsable();
}

bool SharedTextureDMABuf::RetireVulkanPublication() {
  if (!mDMABufInfo.for_webrender) {
    return true;
  }
  if (!mSurface->TryRetireAccess()) {
    return false;
  }
  auto descriptor = mSurfaceDescriptor;
  descriptor.vulkanImageState() = Nothing();
  descriptor.semaphoreFd() = nullptr;
  descriptor.semaphoreFdIsSyncFd() = false;
  RefPtr<DMABufSurface> surface = DMABufSurface::CreateDMABufSurface(
      layers::SurfaceDescriptor(std::move(descriptor)));
  if (!surface || !surface->CreateAccessLock()) {
    return false;
  }
  ClearTextureHost();
  mSurface = std::move(surface);
  return true;
}

void SharedTextureDMABuf::CleanForRecycling() {
  SharedTexture::CleanForRecycling();
  if (mDMABufInfo.for_webrender) {
    ClearTextureHost();
  }
  mSemaphoreFd = nullptr;
  mVulkanGeneration = 0;
}

Maybe<layers::SurfaceDescriptor> SharedTextureDMABuf::ToSurfaceDescriptor() {
  MOZ_ASSERT(mSubmissionIndex > 0);

  layers::SurfaceDescriptor sd;
  if (!mSurface->Serialize(sd)) {
    return Nothing();
  }

  if (sd.type() != layers::SurfaceDescriptor::TSurfaceDescriptorDMABuf) {
    return Nothing();
  }

  auto& sdDMABuf = sd.get_SurfaceDescriptorDMABuf();
  sdDMABuf.semaphoreFd() = mSemaphoreFd;
  if (mDMABufInfo.for_webrender) {
    if (!mVulkanGeneration || !mSurface->AccessLockUsable()) {
      return Nothing();
    }
    nsTArray<uint8_t> device;
    nsTArray<uint8_t> driver;
    device.AppendElements(mDMABufInfo.device_uuid, 16);
    driver.AppendElements(mDMABufInfo.driver_uuid, 16);
    sdDMABuf.vulkanImageState() = Some(
        layers::VulkanImageState(device, driver, mVulkanGeneration,
                                 WrapNotNull(mSurface->GetAccessLockFd())));
    sdDMABuf.semaphoreFdIsSyncFd() = true;
  }

  return Some(sd);
}

void SharedTextureDMABuf::GetSnapshot(const ipc::Shmem& aDestShmem,
                                      size_t aDestStride) {
  if (mDMABufInfo.for_webrender) {
    if (!mVulkanGeneration || !mSurface->WaitForAccess(5000)) {
      memset(aDestShmem.get<uint8_t>(), 0, aDestShmem.Size<uint8_t>());
      return;
    }
    bool complete = false;
    auto unlock = MakeScopeExit([&] { mSurface->UnlockAccess(!complete); });
    wr::WrHalDmaBuf image{};
    image.fd = mSurfaceDescriptor.fds()[0]->GetHandle();
    image.ready_fd = mSemaphoreFd ? mSemaphoreFd->GetHandle() : -1;
    image.width = mWidth;
    image.height = mHeight;
    image.format =
        mDMABufInfo.is_rgba ? wr::ImageFormat::RGBA8 : wr::ImageFormat::BGRA8;
    image.modifier = mDMABufInfo.modifier;
    image.stride = mDMABufInfo.strides[0];
    image.offset = mDMABufInfo.offsets[0];
    std::copy_n(mDMABufInfo.device_uuid, 16, image.device_uuid);
    std::copy_n(mDMABufInfo.driver_uuid, 16, image.driver_uuid);
    complete =
        wr::wr_snapshot_vulkan_dmabuf(&image, aDestShmem.get<uint8_t>(),
                                      aDestShmem.Size<uint8_t>(), aDestStride);
    if (!complete) {
      memset(aDestShmem.get<uint8_t>(), 0, aDestShmem.Size<uint8_t>());
      gfxCriticalNoteOnce << "Vulkan DMA-BUF snapshot failed";
    }
    return;
  }
  const RefPtr<gfx::SourceSurface> surface = mSurface->GetAsSourceSurface();
  if (!surface) {
    MOZ_ASSERT_UNREACHABLE("unexpected to be called");
    gfxCriticalNoteOnce << "Failed to get SourceSurface from DMABufSurface";
    return;
  }

  const RefPtr<gfx::DataSourceSurface> dataSurface = surface->GetDataSurface();
  if (!dataSurface) {
    MOZ_ASSERT_UNREACHABLE("unexpected to be called");
    return;
  }

  gfx::DataSourceSurface::ScopedMap map(dataSurface,
                                        gfx::DataSourceSurface::READ);
  if (!map.IsMapped()) {
    MOZ_ASSERT_UNREACHABLE("unexpected to be called");
    return;
  }

  uint8_t* src = static_cast<uint8_t*>(map.GetData());
  uint8_t* dst = aDestShmem.get<uint8_t>();

  const size_t src_stride = static_cast<size_t>(map.GetStride());
  const size_t bytesPerRow = static_cast<size_t>(mWidth) * 4;
  MOZ_RELEASE_ASSERT(src_stride >= bytesPerRow);
  MOZ_RELEASE_ASSERT(aDestStride >= bytesPerRow);

  for (uint32_t y = 0; y < mHeight; y++) {
    memcpy(dst, src, bytesPerRow);
    if (bytesPerRow < aDestStride) {
      memset(dst + bytesPerRow, 0, aDestStride - bytesPerRow);
    }
    src += src_stride;
    dst += aDestStride;
  }
}

bool SharedTextureDMABuf::PrepareForVulkanPresent(
    const ffi::WGPUGlobal* aContext, RawId aDeviceId, RawId aQueueId,
    RawId aTextureId, uint64_t aGeneration) {
  if (!mDMABufInfo.for_webrender) {
    return true;
  }
  if (!aGeneration || mVulkanGeneration || !mSurface->AccessLockUsable()) {
    return false;
  }
  int32_t fd = -2;
  uint64_t serial = ffi::wgpu_vkimage_prepare_webrender_present(
      aContext, aDeviceId, aQueueId, aTextureId, &fd);
  if (!serial || fd < -1) {
    return false;
  }
  if (fd >= 0) {
    mSemaphoreFd = new gfx::FileHandleWrapper(UniqueFileHandle(fd));
  }
  mVulkanGeneration = aGeneration;
  SetSubmissionIndex(serial);
  return true;
}

ffi::WGPUDMABufInfo SharedTextureDMABuf::GetDMABufInfo() const {
  auto info = mDMABufInfo;
  if (info.for_webrender && !mSurface->AccessLockUsable()) {
    info.is_valid = false;
  }
  return info;
}

UniqueFileHandle SharedTextureDMABuf::CloneDmaBufFd() {
  if (mDMABufInfo.for_webrender && !mSurface->AccessLockUsable()) {
    return UniqueFileHandle();
  }
  return mSurfaceDescriptor.fds()[0]->ClonePlatformHandle();
}

void SharedTextureDMABuf::onBeforeQueueSubmit(
    const ffi::WGPUGlobal* aContext, RawId aDeviceId, RawId aQueueId,
    nsTArray<ffi::WGPUVkSemaphoreHandle>& aSignalSemaphores) {
  SharedTexture::onBeforeQueueSubmit(aContext, aDeviceId, aQueueId,
                                     aSignalSemaphores);
  if (mDMABufInfo.for_webrender) {
    return;
  }

  int32_t rawFd = -1;
  auto semaphore = ffi::wgpu_vksemaphore_create_signal_semaphore(
      aContext, aDeviceId, aQueueId, &rawFd);
  if (!semaphore) {
    gfxCriticalNoteOnce << "Failed to create VkSemaphore";
    return;
  }

  // Ownership transfers to wgpu_server_queue_submit(), which destroys the
  // semaphore once the submission that signals it has completed.
  aSignalSemaphores.AppendElement(semaphore);

  if (rawFd < 0) {
    gfxCriticalNoteOnce << "Failed to get fd from VkSemaphore";
    return;
  }

  mSemaphoreFd = new gfx::FileHandleWrapper(UniqueFileHandle(rawFd));
}

}  // namespace mozilla::webgpu
