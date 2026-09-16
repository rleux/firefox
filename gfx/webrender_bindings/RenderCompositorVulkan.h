/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_RENDERCOMPOSITOR_VULKAN_H
#define MOZILLA_GFX_RENDERCOMPOSITOR_VULKAN_H

#include "RenderCompositor.h"

namespace mozilla::wr {

class RenderCompositorVulkan final : public RenderCompositor {
 public:
  static bool IsRequested();
  static UniquePtr<RenderCompositor> Create(
      const RefPtr<widget::CompositorWidget>& aWidget, nsACString& aError);
  RenderCompositorVulkan(const RefPtr<widget::CompositorWidget>& aWidget,
                         const WrHalSurface& aSurface);
  bool GetHalSurface(WrHalSurface* aSurface) const override;
  void SetRenderer(Renderer* aRenderer) override { mRenderer = aRenderer; }
  bool BeginFrame() override;
  void CancelFrame() override;
  RenderedFrameId EndFrame(const nsTArray<DeviceIntRect>& aDirtyRects) override;
  bool WaitForGPU() override;
  RenderedFrameId GetLastCompletedFrameId() override;
  bool MakeCurrent() override { return true; }
  gfx::DeviceResetReason IsContextLost(bool aForce) override;
  void Pause() override;
  bool Resume() override;
  bool IsPaused() override { return mPaused; }
  void Update() override;
  LayoutDeviceIntSize GetBufferSize() override;
  bool SurfaceOriginIsTopLeft() override { return true; }

 private:
  WrHalSurface mSurface;
  // RendererOGL owns this renderer and deletes it before this compositor.
  Renderer* mRenderer = nullptr;
  uint64_t mCompletedFrame = 1;
  bool mPaused = false;
  bool mFailed = false;
};

}  // namespace mozilla::wr
#endif
