/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_RENDERCOMPOSITOR_VULKAN_H
#define MOZILLA_GFX_RENDERCOMPOSITOR_VULKAN_H

#include "RenderCompositor.h"
#include "base/timer.h"
#ifdef MOZ_X11
#  include "X11WindowVisibility.h"
#endif

class DMABufSurfaceYUV;

namespace mozilla::gfx {
class VulkanVideoCapabilities;
}

namespace mozilla::wr {

class RenderCompositorVulkan final : public RenderCompositor {
 public:
  static bool IsRequested();
  static bool SupportsWebGL();
  static gfx::VulkanVideoCapabilities ProbeVideoCapabilities();
  static bool SupportsVideo();
  static bool SupportsRetainedVideo(const DMABufSurfaceYUV& aSurface);
  static void DisableVideo();
  static UniquePtr<RenderCompositor> Create(
      const RefPtr<widget::CompositorWidget>& aWidget, nsACString& aError);
  RenderCompositorVulkan(const RefPtr<widget::CompositorWidget>& aWidget,
                         const WrHalSurface& aSurface);
  bool GetHalSurface(WrHalSurface* aSurface) const override;
  void SetRenderer(Renderer* aRenderer) override;
  bool BeginFrame() override;
  void CancelFrame() override;
  RenderedFrameId EndFrame(const nsTArray<DeviceIntRect>& aDirtyRects) override;
  bool WaitForGPU() override;
  RenderedFrameId GetLastCompletedFrameId() override;
  RenderedFrameId UpdateFrameId() override;
  bool MakeCurrent() override { return true; }
  bool IsWindowHidden() override;
  gfx::DeviceResetReason IsContextLost(bool aForce) override;
  void Pause() override;
  bool Resume() override;
  bool IsPaused() override { return mPaused; }
  void Update() override;
  LayoutDeviceIntSize GetBufferSize() override;
  bool SurfaceOriginIsTopLeft() override { return true; }

 private:
  bool PollCompletions(bool aNotify);
  void PollPendingFrames();
  WrHalSurface mSurface;
#ifdef MOZ_X11
  X11WindowVisibility mWindowVisibility;
  bool mWindowWasHidden = false;
#endif
  // RendererOGL owns this renderer and deletes it before this compositor.
  Renderer* mRenderer = nullptr;
  uint64_t mCompletedFrame = 1;
  uint64_t mSubmittedFrame = 1;
  base::RepeatingTimer<RenderCompositorVulkan> mCompletionTimer;
  bool mPaused = false;
  bool mFailed = false;
};

}  // namespace mozilla::wr
#endif
