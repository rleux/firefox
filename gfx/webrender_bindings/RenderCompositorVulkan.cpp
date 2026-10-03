/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderCompositorVulkan.h"

#include "mozilla/webrender/RenderThread.h"
#include "mozilla/widget/CompositorWidget.h"

namespace mozilla::wr {

RenderCompositorVulkan::RenderCompositorVulkan(
    const RefPtr<widget::CompositorWidget>& aWidget,
    const WrVulkanConfig& aConfig)
    : RenderCompositor(aWidget) {
  mConfig.emplace(aConfig);
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
  if (!wr_renderer_set_vulkan_surface(mRenderer,
                                      next ? &next->Raw() : nullptr)) {
    Fail();
    return false;
  }
  mConfig.reset();
  if (next) {
    mConfig.emplace(std::move(next.ref()));
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
  if (!mRenderer || mFailed || !mConfig || !mConfig->HasWindow()) {
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
