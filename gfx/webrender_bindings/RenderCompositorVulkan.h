/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_RENDERCOMPOSITOR_VULKAN_H
#define MOZILLA_GFX_RENDERCOMPOSITOR_VULKAN_H

#include <deque>

#include "RenderCompositor.h"
#include "base/timer.h"
#include "mozilla/Maybe.h"

namespace mozilla::wr {

#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
class VulkanX11Display;
#endif

class OwnedVulkanConfig {
 public:
  explicit OwnedVulkanConfig(const WrVulkanConfig& aConfig) : mConfig(aConfig) {
    Retain(mConfig.display_owner);
    Retain(mConfig.window_owner);
  }
  OwnedVulkanConfig(const OwnedVulkanConfig&) = delete;
  OwnedVulkanConfig& operator=(const OwnedVulkanConfig&) = delete;
  OwnedVulkanConfig(OwnedVulkanConfig&& aOther) : mConfig(aOther.mConfig) {
    aOther.mConfig.window_owner = {};
    aOther.mConfig.display_owner = {};
  }
  ~OwnedVulkanConfig() {
    Release(mConfig.window_owner);
    Release(mConfig.display_owner);
  }
  const WrVulkanConfig& Raw() const { return mConfig; }
  bool HasWindow() const { return mConfig.window_owner.object != nullptr; }

 private:
  static void Retain(const WrVulkanOwner& aOwner) {
    MOZ_RELEASE_ASSERT(aOwner.object ? aOwner.retain && aOwner.release
                                     : !aOwner.retain && !aOwner.release);
    if (aOwner.object) {
      aOwner.retain(aOwner.object);
    }
  }
  static void Release(const WrVulkanOwner& aOwner) {
    if (aOwner.object) {
      aOwner.release(aOwner.object);
    }
  }

  WrVulkanConfig mConfig;
};

class VulkanFrameTracker {
 public:
  bool Poll(const GpuSubmissionStatus& aStatus) {
    if (aStatus.completed > aStatus.submitted ||
        aStatus.submitted < mSubmitted || aStatus.completed < mCompleted) {
      return false;
    }
    mSubmitted = aStatus.submitted;
    mCompleted = aStatus.completed;
    while (!mPending.empty() && mPending.front().mSerial <= mCompleted) {
      mCompletedFrame = mPending.front().mFrame;
      mPending.pop_front();
    }
    return true;
  }

  bool AddFrame(RenderedFrameId aFrame, const GpuSubmissionStatus& aStatus) {
    if (aFrame <= mLatestFrame || !Poll(aStatus)) {
      return false;
    }
    mLatestFrame = aFrame;
    if (mSubmitted == mCompleted) {
      mCompletedFrame = aFrame;
    } else if (!mPending.empty() && mPending.back().mSerial == mSubmitted) {
      mPending.back().mFrame = aFrame;
    } else {
      mPending.push_back({aFrame, mSubmitted});
    }
    return true;
  }

  RenderedFrameId CompletedFrame() const { return mCompletedFrame; }
  bool HasPendingFrames() const { return !mPending.empty(); }

 private:
  struct PendingFrame {
    RenderedFrameId mFrame;
    uint64_t mSerial;
  };
  std::deque<PendingFrame> mPending;
  uint64_t mSubmitted = 0;
  uint64_t mCompleted = 0;
  RenderedFrameId mLatestFrame{1};
  RenderedFrameId mCompletedFrame{1};
};

class RenderCompositorVulkan final : public RenderCompositor {
 public:
  static bool IsRequested();
  static UniquePtr<RenderCompositor> Create(
      const RefPtr<widget::CompositorWidget>& aWidget, nsACString& aError);

  RenderCompositorVulkan(const RefPtr<widget::CompositorWidget>& aWidget,
                         const WrVulkanConfig& aConfig);
  ~RenderCompositorVulkan() override;

  const WrVulkanConfig* GetVulkanConfig() const override {
    return mConfig ? &mConfig->Raw() : nullptr;
  }
  bool UsesBackendPresentation() const override { return true; }
  bool SetSurface(const WrVulkanConfig* aConfig);
  void SetRenderer(Renderer* aRenderer, WindowId aWindowId) override;
  bool BeginFrame() override;
  RenderedFrameId EndFrame(const nsTArray<DeviceIntRect>& aDirtyRects) override;
  RenderedFrameId UpdateFrameId() override;
  bool WaitForGPU() override;
  RenderedFrameId GetLastCompletedFrameId() override;
  bool MakeCurrent() override { return true; }
  gfx::DeviceResetReason IsContextLost(bool aForce) override;
  void Pause() override;
  bool Resume() override;
  bool IsPaused() override {
    return mPaused || !mConfig || !mConfig->HasWindow();
  }
  void Update() override;
  LayoutDeviceIntSize GetBufferSize() override;
  bool SurfaceOriginIsTopLeft() override { return true; }
  bool SupportAsyncScreenshot() override { return false; }

 private:
  bool PollCompletions();
  void PollPendingFrames();
  void Fail();
  void WakeUp();

  Maybe<OwnedVulkanConfig> mConfig;
  VulkanFrameTracker mFrames;
  // RendererOGL clears this borrowed pointer immediately after Renderer deletion.
  Renderer* mRenderer = nullptr;
  WindowId mWindowId{};
  base::RepeatingTimer<RenderCompositorVulkan> mCompletionTimer;
  bool mPaused = false;
  bool mFailed = false;
  bool mValidation = false;
  bool mVsync = true;
  bool mTransparent = false;
#ifdef MOZ_WIDGET_ANDROID
  bool mHandlingNewSurfaceError = false;
#endif
#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
  RefPtr<VulkanX11Display> mX11Display;
#endif
};

}  // namespace mozilla::wr
#endif
