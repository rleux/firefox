/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderCompositorVulkan.h"

#include <sys/stat.h>
#include <sys/sysmacros.h>

#include <algorithm>
#include <array>
#include <cstring>

#include "mozilla/StaticMutex.h"
#include "mozilla/StaticPtr.h"
#include "mozilla/gfx/gfxVars.h"
#include "mozilla/widget/DMABufSurface.h"
#include "mozilla/widget/GtkCompositorWidget.h"
#include "prenv.h"
#ifdef MOZ_X11
#  include "mozilla/X11Util.h"
#endif

namespace mozilla::wr {

namespace {
struct DmaBufDevice {
  std::array<uint8_t, 16> mDevice;
  std::array<uint8_t, 16> mDriver;
  nsTArray<uint64_t> mRGBA;
  nsTArray<uint64_t> mBGRA;
  WrHalNv12Capabilities mVideo;
};
StaticMutex sDmaBufDevicesMutex;
StaticAutoPtr<nsTArray<DmaBufDevice*>> sDmaBufDevices;
bool sVideoFailed = false;
}  // namespace

extern "C" void* wr_vulkan_register_dmabuf_device(
    const uint8_t* aDevice, const uint8_t* aDriver, const uint64_t* aRGBA,
    size_t aRGBALength, const uint64_t* aBGRA, size_t aBGRALength,
    const WrHalNv12Capabilities* aVideo) {
  auto device = MakeUnique<DmaBufDevice>();
  std::copy_n(aDevice, 16, device->mDevice.begin());
  std::copy_n(aDriver, 16, device->mDriver.begin());
  device->mRGBA.AppendElements(aRGBA, aRGBALength);
  device->mBGRA.AppendElements(aBGRA, aBGRALength);
  device->mVideo = *aVideo;
  StaticMutexAutoLock lock(sDmaBufDevicesMutex);
  if (!sDmaBufDevices) {
    sDmaBufDevices = new nsTArray<DmaBufDevice*>();
  }
  sDmaBufDevices->AppendElement(device.get());
  return device.release();
}

extern "C" void wr_vulkan_unregister_dmabuf_device(void* aRegistration) {
  UniquePtr<DmaBufDevice> device(static_cast<DmaBufDevice*>(aRegistration));
  StaticMutexAutoLock lock(sDmaBufDevicesMutex);
  MOZ_RELEASE_ASSERT(sDmaBufDevices &&
                     sDmaBufDevices->RemoveElement(device.get()));
  if (sDmaBufDevices->IsEmpty()) {
    sDmaBufDevices = nullptr;
  }
}

extern "C" bool wr_vulkan_supports_dmabuf(const uint8_t* aDevice,
                                          const uint8_t* aDriver, bool aRGBA,
                                          uint64_t aModifier) {
  StaticMutexAutoLock lock(sDmaBufDevicesMutex);
  if (!sDmaBufDevices || sDmaBufDevices->IsEmpty()) {
    return false;
  }
  for (const auto* device : *sDmaBufDevices) {
    if (memcmp(device->mDevice.data(), aDevice, 16) ||
        memcmp(device->mDriver.data(), aDriver, 16) ||
        !(aRGBA ? device->mRGBA : device->mBGRA).Contains(aModifier)) {
      return false;
    }
  }
  return true;
}

extern "C" bool wr_vulkan_supports_foreign_webgl(uint64_t aMajor,
                                                 uint64_t aMinor);

bool RenderCompositorVulkan::SupportsWebGL() {
  nsCString node(PR_GetEnv("MOZ_DRM_DEVICE"));
  if (node.IsEmpty()) {
    node = gfx::gfxVars::DrmRenderDevice();
  }
  struct stat device;
  return !node.IsEmpty() && !stat(node.get(), &device) &&
         S_ISCHR(device.st_mode) &&
         wr_vulkan_supports_foreign_webgl(major(device.st_rdev),
                                          minor(device.st_rdev));
}

bool RenderCompositorVulkan::IsRequested() {
  return gfx::gfxVars::UseWebRenderVulkan() &&
         !gfx::gfxVars::UseSoftwareWebRender();
}

gfx::VulkanVideoCapabilities RenderCompositorVulkan::ProbeVideoCapabilities() {
  MOZ_ASSERT(NS_IsMainThread());
  gfx::VulkanVideoCapabilities result;
  if (!IsRequested() || sVideoFailed) return result;
  nsCString node(PR_GetEnv("MOZ_DRM_DEVICE"));
  if (node.IsEmpty()) node = gfx::gfxVars::DrmRenderDevice();
  struct stat device;
  if (node.IsEmpty() || stat(node.get(), &device) || !S_ISCHR(device.st_mode)) {
    return result;
  }
  WrHalNv12Capabilities capabilities{};
  if (!wr_vulkan_query_nv12(major(device.st_rdev), minor(device.st_rdev),
                            &capabilities) ||
      !capabilities.format_count ||
      capabilities.format_count > std::size(capabilities.formats)) {
    return result;
  }
  result.drmMajor() = major(device.st_rdev);
  result.drmMinor() = minor(device.st_rdev);
  result.deviceUUID().AppendElements(capabilities.device_uuid, 16);
  result.driverUUID().AppendElements(capabilities.driver_uuid, 16);
  for (size_t i = 0; i < capabilities.format_count; ++i) {
    const auto& format = capabilities.formats[i];
    result.formats().AppendElement(
        gfx::VulkanVideoFormat(format.modifier, format.max_width,
                               format.max_height, format.max_allocation_size));
  }
  return result;
}

bool RenderCompositorVulkan::SupportsVideo() {
  if (!IsRequested() || !gfx::gfxVars::UseWebRenderVulkanVideo()) return false;
  const auto& expected = gfx::gfxVars::WebRenderVulkanVideoCapabilities();
  if (expected.formats().IsEmpty() || expected.deviceUUID().Length() != 16 ||
      expected.driverUUID().Length() != 16) {
    return false;
  }
  StaticMutexAutoLock lock(sDmaBufDevicesMutex);
  if (!sDmaBufDevices || sDmaBufDevices->IsEmpty()) return false;
  for (const auto* device : *sDmaBufDevices) {
    const auto& video = device->mVideo;
    if (video.format_count > std::size(video.formats) ||
        video.drm_node[0] != expected.drmMajor() ||
        video.drm_node[1] != expected.drmMinor() ||
        memcmp(video.device_uuid, expected.deviceUUID().Elements(), 16) ||
        memcmp(video.driver_uuid, expected.driverUUID().Elements(), 16)) {
      return false;
    }
    for (const auto& format : expected.formats()) {
      bool supported = false;
      for (size_t i = 0; i < video.format_count; ++i) {
        const auto& actual = video.formats[i];
        supported |= actual.modifier == format.modifier() &&
                     actual.max_width >= format.maxWidth() &&
                     actual.max_height >= format.maxHeight() &&
                     actual.max_allocation_size >= format.maxAllocationSize();
      }
      if (!supported) return false;
    }
  }
  return true;
}

bool RenderCompositorVulkan::SupportsRetainedVideo(
    const DMABufSurfaceYUV& aSurface) {
  if (!IsRequested()) return false;
  StaticMutexAutoLock lock(sDmaBufDevicesMutex);
  if (!sDmaBufDevices || sDmaBufDevices->IsEmpty()) return false;
  for (const auto* device : *sDmaBufDevices) {
    const auto& video = device->mVideo;
    if (!video.format_count || video.format_count > std::size(video.formats))
      return false;
    gfx::VulkanVideoCapabilities capabilities;
    capabilities.drmMajor() = video.drm_node[0];
    capabilities.drmMinor() = video.drm_node[1];
    capabilities.deviceUUID().AppendElements(video.device_uuid, 16);
    capabilities.driverUUID().AppendElements(video.driver_uuid, 16);
    for (size_t i = 0; i < video.format_count; ++i) {
      const auto& format = video.formats[i];
      capabilities.formats().AppendElement(gfx::VulkanVideoFormat(
          format.modifier, format.max_width, format.max_height,
          format.max_allocation_size));
    }
    if (!aSurface.SupportsVAAPIImage(capabilities)) return false;
  }
  return true;
}

void RenderCompositorVulkan::DisableVideo() {
  MOZ_ASSERT(NS_IsMainThread() && XRE_IsParentProcess());
  if (!gfx::gfxVars::UseWebRenderVulkan()) return;
  sVideoFailed = true;
  gfx::gfxVarsCollectUpdates collect;
  gfx::gfxVars::SetUseWebRenderVulkanVideo(false);
  gfx::gfxVars::SetWebRenderVulkanVideoCapabilities(
      gfx::VulkanVideoCapabilities());
}

UniquePtr<RenderCompositor> RenderCompositorVulkan::Create(
    const RefPtr<widget::CompositorWidget>& aWidget, nsACString& aError) {
#ifdef MOZ_X11
  auto* gtk = aWidget->AsGTK();
  auto* display = DefaultXDisplay();
  if (gtk && display && gtk->XWindow()) {
    XWindowAttributes attributes{};
    if (!XGetWindowAttributes(display, gtk->XWindow(), &attributes)) {
      aError.AssignLiteral("Unable to query Vulkan window attributes");
      return nullptr;
    }
    WrHalSurface surface{display, gtk->XWindow(), DefaultScreen(display), false,
                         attributes.depth == 32};
    return MakeUnique<RenderCompositorVulkan>(aWidget, surface);
  }
#endif
  aError.AssignLiteral("Vulkan WebRender requires an available X11 window");
  return nullptr;
}

RenderCompositorVulkan::RenderCompositorVulkan(
    const RefPtr<widget::CompositorWidget>& aWidget,
    const WrHalSurface& aSurface)
    : RenderCompositor(aWidget), mSurface(aSurface) {}

bool RenderCompositorVulkan::GetHalSurface(WrHalSurface* aSurface) const {
  *aSurface = mSurface;
  return true;
}

bool RenderCompositorVulkan::BeginFrame() {
  if (!mRenderer || mPaused || mFailed) {
    return false;
  }
  auto size = GetBufferSize();
  if (size.width <= 0 || size.height <= 0) {
    return false;
  }
  return wr_renderer_vulkan_begin(mRenderer, size.width, size.height);
}

void RenderCompositorVulkan::CancelFrame() {
  if (mRenderer && !mFailed) {
    mFailed = !wr_renderer_vulkan_cancel(mRenderer);
  }
}

RenderedFrameId RenderCompositorVulkan::EndFrame(
    const nsTArray<DeviceIntRect>&) {
  return UpdateFrameId();
}

RenderedFrameId RenderCompositorVulkan::UpdateFrameId() {
  auto id = GetNextRenderFrameId();
  if (mRenderer && !mFailed && !wr_renderer_vulkan_end(mRenderer, id.mId)) {
    mFailed = true;
  }
  return id;
}

bool RenderCompositorVulkan::WaitForGPU() {
  if (mRenderer && !mFailed) {
    mFailed = !wr_renderer_vulkan_poll(mRenderer, &mCompletedFrame);
  }
  return !mFailed;
}

RenderedFrameId RenderCompositorVulkan::GetLastCompletedFrameId() {
  WaitForGPU();
  return RenderedFrameId{mCompletedFrame};
}

gfx::DeviceResetReason RenderCompositorVulkan::IsContextLost(bool) {
  return mFailed || (mRenderer && wr_renderer_vulkan_failed(mRenderer))
             ? gfx::DeviceResetReason::DRIVER_ERROR
             : gfx::DeviceResetReason::OK;
}

void RenderCompositorVulkan::Pause() {
  mPaused = true;
  if (mRenderer && !mFailed) {
    mFailed = !wr_renderer_vulkan_pause(mRenderer);
  }
}

bool RenderCompositorVulkan::Resume() {
  mPaused = false;
  if (mRenderer) {
    wr_renderer_force_redraw(mRenderer);
  }
  return !mFailed;
}

void RenderCompositorVulkan::Update() { WaitForGPU(); }

LayoutDeviceIntSize RenderCompositorVulkan::GetBufferSize() {
  return mWidget->GetClientSize();
}

}  // namespace mozilla::wr
