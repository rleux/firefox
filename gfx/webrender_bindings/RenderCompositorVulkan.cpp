/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderCompositorVulkan.h"

#include "mozilla/StaticPrefs_gfx.h"
#include "mozilla/gfx/gfxVars.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/widget/CompositorWidget.h"

#ifdef MOZ_WIDGET_ANDROID
#  include <android/native_window.h>
#  include <android/native_window_jni.h>

#  include "mozilla/ScopeExit.h"
#  include "mozilla/jni/Utils.h"
#  include "mozilla/widget/AndroidCompositorWidget.h"
#endif

namespace mozilla::wr {

bool RenderCompositorVulkan::IsRequested() {
  return gfx::gfxVars::UseWebRenderVulkan() &&
         !gfx::gfxVars::UseSoftwareWebRender();
}

#ifdef MOZ_WIDGET_ANDROID
static Maybe<OwnedVulkanConfig> AcquireAndroidSurface(
    widget::AndroidCompositorWidget* aWidget, bool aValidation, bool aVsync,
    bool aTransparent) {
  auto surface = reinterpret_cast<jobject>(aWidget->GetEGLNativeWindow());
  if (!surface) {
    return Nothing();
  }
  auto* window = ANativeWindow_fromSurface(jni::GetEnvForThread(), surface);
  if (!window) {
    return Nothing();
  }
  auto release = MakeScopeExit([&] { ANativeWindow_release(window); });
  WrVulkanConfig config{
      WrWindowHandle::Android(window),
      {window,
       [](void* aWindow) {
         ANativeWindow_acquire(static_cast<ANativeWindow*>(aWindow));
       },
       [](void* aWindow) {
         ANativeWindow_release(static_cast<ANativeWindow*>(aWindow));
       }},
      {},
      aValidation,
      aVsync,
      aTransparent};
  return Some(OwnedVulkanConfig(config));
}
#endif

UniquePtr<RenderCompositor> RenderCompositorVulkan::Create(
    const RefPtr<widget::CompositorWidget>& aWidget, nsACString& aError) {
#ifdef MOZ_WIDGET_ANDROID
  if (aWidget && aWidget->AsAndroid()) {
    WrVulkanConfig config{
        WrWindowHandle::Android(nullptr), {}, {}, false, true, false};
    return MakeUnique<RenderCompositorVulkan>(aWidget, config);
  }
#endif
  aError.AssignLiteral("RcVulkan(unsupported widget)");
  return nullptr;
}

RenderCompositorVulkan::RenderCompositorVulkan(
    const RefPtr<widget::CompositorWidget>& aWidget,
    const WrVulkanConfig& aConfig)
    : RenderCompositor(aWidget) {
  mConfig.emplace(aConfig);
#ifdef MOZ_WIDGET_ANDROID
  mValidation = aConfig.validation;
  mVsync = aConfig.vsync;
  mTransparent = aConfig.transparent;
#endif
}

RenderCompositorVulkan::~RenderCompositorVulkan() {
  MOZ_ASSERT(!mRenderer);
  mCompletionTimer.Stop();
}

void RenderCompositorVulkan::SetRenderer(Renderer* aRenderer,
                                         WindowId aWindowId) {
  MOZ_ASSERT(!mRenderer || !aRenderer);
  mCompletionTimer.Stop();
  mRenderer = aRenderer;
  mWindowId = aWindowId;
}

bool RenderCompositorVulkan::SetSurface(const WrVulkanConfig* aConfig) {
  if (!mRenderer || mFailed) {
    return false;
  }
  Maybe<OwnedVulkanConfig> next;
  if (aConfig) {
    next.emplace(*aConfig);
  }
  if (!wr_renderer_set_wgpu_surface(mRenderer,
                                      next ? &next->Raw() : nullptr)) {
    Fail();
    return false;
  }
  mConfig.reset();
  if (next) {
    mConfig.emplace(std::move(next.ref()));
#ifdef MOZ_WIDGET_ANDROID
    mVsync = mConfig->Raw().vsync;
    mTransparent = mConfig->Raw().transparent;
#endif
  }
  auto previous = mFrames.CompletedFrame();
  if (!PollCompletions()) {
    return false;
  }
  if (previous != mFrames.CompletedFrame()) {
    WakeUp();
  }
  return true;
}

bool RenderCompositorVulkan::BeginFrame() {
  return mRenderer && !IsPaused() && !mFailed && !GetBufferSize().IsEmpty();
}

RenderedFrameId RenderCompositorVulkan::EndFrame(
    const nsTArray<DeviceIntRect>&) {
  return UpdateFrameId();
}

RenderedFrameId RenderCompositorVulkan::UpdateFrameId() {
  auto frame = GetNextRenderFrameId();
  if (!mRenderer || mFailed) {
    return frame;
  }
  GpuSubmissionStatus status{};
  if (wr_renderer_gpu_submission_status(mRenderer, &status) !=
          WrGpuSubmissionResult::Available ||
      !mFrames.AddFrame(frame, status)) {
    Fail();
    return frame;
  }
  if (!mFrames.HasPendingFrames()) {
    mCompletionTimer.Stop();
  } else if (!mCompletionTimer.IsRunning()) {
    mCompletionTimer.Start(base::TimeDelta::FromMilliseconds(2), this,
                           &RenderCompositorVulkan::PollPendingFrames);
  }
  return frame;
}

bool RenderCompositorVulkan::PollCompletions() {
  if (!mRenderer || mFailed) {
    return false;
  }
  GpuSubmissionStatus status{};
  if (wr_renderer_gpu_submission_status(mRenderer, &status) !=
          WrGpuSubmissionResult::Available ||
      !mFrames.Poll(status)) {
    Fail();
    return false;
  }
  if (!mFrames.HasPendingFrames()) {
    mCompletionTimer.Stop();
  }
  return true;
}

void RenderCompositorVulkan::PollPendingFrames() {
  auto previous = mFrames.CompletedFrame();
  if (PollCompletions() && previous != mFrames.CompletedFrame()) {
    WakeUp();
  }
}

void RenderCompositorVulkan::WakeUp() {
  auto* thread = RenderThread::Get();
  if (thread && !thread->HasShutdown()) {
    thread->WrNotifierEvent_WakeUp(mWindowId, false);
  }
}

void RenderCompositorVulkan::Fail() {
  if (!mFailed) {
    mFailed = true;
    mCompletionTimer.Stop();
    WakeUp();
  }
}

bool RenderCompositorVulkan::WaitForGPU() { return PollCompletions(); }

RenderedFrameId RenderCompositorVulkan::GetLastCompletedFrameId() {
  return mFrames.CompletedFrame();
}

gfx::DeviceResetReason RenderCompositorVulkan::IsContextLost(bool) {
  return mFailed ? gfx::DeviceResetReason::DRIVER_ERROR
                 : gfx::DeviceResetReason::OK;
}

void RenderCompositorVulkan::Pause() {
  mPaused = true;
  if (mRenderer && !mFailed) {
    if (!wr_renderer_set_surface_paused(mRenderer, true)) {
      Fail();
    } else {
      SetSurface(nullptr);
    }
  }
}

bool RenderCompositorVulkan::Resume() {
  if (!mRenderer || mFailed) {
    return false;
  }
#ifdef MOZ_WIDGET_ANDROID
  Pause();
  if (mFailed) {
    return false;
  }
  auto* widget = mWidget ? mWidget->AsAndroid() : nullptr;
  if (!widget) {
    return false;
  }
  auto config =
      AcquireAndroidSurface(widget, mValidation, mVsync, mTransparent);
  if (!config) {
    if (mHandlingNewSurfaceError) {
      RenderThread::Get()->HandleWebRenderError(WebRenderError::NEW_SURFACE);
    }
    mHandlingNewSurfaceError = true;
    return false;
  }
  mHandlingNewSurfaceError = false;
  if (!SetSurface(&config->Raw())) {
    return false;
  }
#endif
  if (!mConfig || !mConfig->HasWindow()) {
    return false;
  }
  if (!wr_renderer_set_surface_paused(mRenderer, false)) {
    Fail();
    return false;
  }
  mPaused = false;
  return true;
}

void RenderCompositorVulkan::Update() { PollCompletions(); }

LayoutDeviceIntSize RenderCompositorVulkan::GetBufferSize() {
  return mWidget->GetClientSize();
}

}  // namespace mozilla::wr
