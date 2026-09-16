/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderCompositorVulkan.h"

#include <algorithm>
#include <array>
#include <cstring>

#include "mozilla/StaticMutex.h"
#include "mozilla/StaticPtr.h"
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
};
StaticMutex sDmaBufDevicesMutex;
StaticAutoPtr<nsTArray<DmaBufDevice*>> sDmaBufDevices;
}  // namespace

extern "C" void* wr_vulkan_register_dmabuf_device(
    const uint8_t* aDevice, const uint8_t* aDriver, const uint64_t* aRGBA,
    size_t aRGBALength, const uint64_t* aBGRA, size_t aBGRALength) {
  auto device = MakeUnique<DmaBufDevice>();
  std::copy_n(aDevice, 16, device->mDevice.begin());
  std::copy_n(aDriver, 16, device->mDriver.begin());
  device->mRGBA.AppendElements(aRGBA, aRGBALength);
  device->mBGRA.AppendElements(aBGRA, aBGRALength);
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

bool RenderCompositorVulkan::IsRequested() {
  const char* backend = PR_GetEnv("MOZ_WR_BACKEND");
  return backend && !strcmp(backend, "vulkan");
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
  auto id = GetNextRenderFrameId();
  if (!wr_renderer_vulkan_end(mRenderer, id.mId)) {
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
