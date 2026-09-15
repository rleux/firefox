/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderCompositorVulkan.h"

#include "mozilla/widget/GtkCompositorWidget.h"
#ifdef MOZ_X11
#  include "mozilla/X11Util.h"
#endif

namespace mozilla::wr {

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
