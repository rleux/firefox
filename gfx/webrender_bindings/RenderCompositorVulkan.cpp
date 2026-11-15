/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderCompositorVulkan.h"
#include "VulkanCompositorSupport.h"

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

#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
#  include "mozilla/WidgetUtilsGtk.h"
#  include "mozilla/X11Util.h"
#  include "mozilla/widget/GtkCompositorWidget.h"
#endif

namespace mozilla::wr {

bool IsVulkanCompositorRequested() {
  return CanUseVulkanCompositor(BuildVulkanCompositorPlatform(),
                                gfx::gfxVars::WebRenderVulkanX11Display(),
                                gfx::gfxVars::UseWebRenderVulkan(),
                                gfx::gfxVars::UseSoftwareWebRender());
}

bool RenderCompositorVulkan::IsRequested() {
  return IsVulkanCompositorRequested();
}

#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
class VulkanX11Display final {
 public:
  NS_INLINE_DECL_REFCOUNTING(VulkanX11Display)

  explicit VulkanX11Display(Display* aDisplay) : mDisplay(aDisplay) {}
  Display* Get() const { return mDisplay; }

 private:
  ~VulkanX11Display() { XCloseDisplay(mDisplay); }
  Display* const mDisplay;
};

static Maybe<OwnedVulkanConfig> AcquireX11Surface(
    widget::CompositorWidget* aWidget, VulkanX11Display* aDisplay,
    bool aValidation, bool aVsync) {
  auto* gtk = aWidget->AsGTK();
  const auto window = gtk ? gtk->VulkanXWindow() : 0;
  if (!window) {
    return Nothing();
  }
  XWindowAttributes attributes{};
  if (!XGetWindowAttributes(aDisplay->Get(), window, &attributes)) {
    return Nothing();
  }
  // GTK synchronously destroys the Renderer before replacing or unrealizing its
  // native window.
  WrVulkanConfig config{
      WrWindowHandle::Xlib(aDisplay->Get(), window,
                           XScreenNumberOfScreen(attributes.screen)),
      {aWidget,
       [](void* aOwner) {
         static_cast<widget::CompositorWidget*>(aOwner)->AddRef();
       },
       [](void* aOwner) {
         static_cast<widget::CompositorWidget*>(aOwner)->Release();
       }},
      {aDisplay,
       [](void* aOwner) { static_cast<VulkanX11Display*>(aOwner)->AddRef(); },
       [](void* aOwner) { static_cast<VulkanX11Display*>(aOwner)->Release(); }},
      aValidation,
      aVsync,
      gtk->VulkanNeedsAlpha()};
  return Some(OwnedVulkanConfig(config));
}
#endif

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
    return std::make_unique<RenderCompositorVulkan>(aWidget, config);
  }
#endif
#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
  if (aWidget && aWidget->AsGTK() && widget::GdkIsX11Display()) {
    auto* connection = XOpenDisplay(XDisplayString(DefaultXDisplay()));
    if (!connection) {
      aError.AssignLiteral("RcVulkan(X11 display)");
      return nullptr;
    }
    RefPtr<VulkanX11Display> display = new VulkanX11Display(connection);
    auto config = AcquireX11Surface(aWidget, display, false, true);
    if (!config) {
      aError.AssignLiteral("RcVulkan(X11 window)");
      return nullptr;
    }
    auto compositor =
        std::make_unique<RenderCompositorVulkan>(aWidget, config->Raw());
    compositor->mX11Display = std::move(display);
    return compositor;
  }
#endif
#ifdef XP_WIN
  // shortcut: no HWND retirement lease or DXGI CPU host; keep Gecko disabled
  // until explicit/parent window destruction and video fallback are supported.
  aError.AssignLiteral(
      "RcVulkan(Windows HWND lifetime and DXGI CPU hosts unsupported)");
#else
  aError.AssignLiteral("RcVulkan(unsupported widget)");
#endif
  return nullptr;
}

RenderCompositorVulkan::RenderCompositorVulkan(
    const RefPtr<widget::CompositorWidget>& aWidget,
    const WrVulkanConfig& aConfig)
    : RenderCompositor(aWidget) {
  mConfig.emplace(aConfig);
  UpdateWindowVisibility();
  mValidation = aConfig.validation;
  mVsync = aConfig.vsync;
  mTransparent = aConfig.transparent;
}

RenderCompositorVulkan::~RenderCompositorVulkan() {
  MOZ_ASSERT(!mRenderer);
  mCompletionTimer.Stop();
}

void RenderCompositorVulkan::UpdateWindowVisibility() {
#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
  mWindowVisibility.reset();
  mWindowWasHidden = false;
  if (mConfig && mConfig->Raw().window.IsXlib()) {
    const auto& window = mConfig->Raw().window.xlib;
    mWindowVisibility.emplace(window.display, window.window);
  }
#endif
}

bool RenderCompositorVulkan::IsWindowHidden() {
  if (!mRenderer || mFailed || IsPaused()) {
    return false;
  }
#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
  const bool hidden =
      mWindowVisibility &&
      mWindowVisibility->Query() == X11WindowVisibility::State::Hidden;
  if (mWindowWasHidden && !hidden) {
    wr_renderer_force_redraw(mRenderer);
  }
  mWindowWasHidden = hidden;
  return hidden;
#else
  return false;
#endif
}

void RenderCompositorVulkan::SetRenderer(Renderer* aRenderer,
                                         WindowId aWindowId) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  MOZ_ASSERT(!mRenderer || !aRenderer);
  mCompletionTimer.Stop();
  mRenderer = aRenderer;
  mWindowId = aWindowId;
}

bool RenderCompositorVulkan::SetSurface(const WrVulkanConfig* aConfig) {
  if (!mRenderer || (mFailed && aConfig)) {
    return false;
  }
  Maybe<OwnedVulkanConfig> next;
  if (aConfig) {
    next.emplace(*aConfig);
  }
  if (!wr_renderer_set_wgpu_surface(mRenderer, next ? &next->Raw() : nullptr)) {
    if (!aConfig) {
      mConfig.reset();
    }
    Fail();
    return false;
  }
  mConfig.reset();
  if (next) {
    mConfig.emplace(std::move(next.ref()));
    mVsync = mConfig->Raw().vsync;
    mTransparent = mConfig->Raw().transparent;
  }
  UpdateWindowVisibility();
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
  if (mRenderer) {
    const bool paused = wr_renderer_set_surface_paused(mRenderer, true);
    SetSurface(nullptr);
    if (!paused) {
      Fail();
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
#elif defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
  if (mX11Display) {
    Pause();
    if (mFailed) {
      return false;
    }
    auto config = AcquireX11Surface(mWidget, mX11Display, mValidation, mVsync);
    if (!config) {
      RenderThread::Get()->HandleWebRenderError(WebRenderError::NEW_SURFACE);
      return false;
    }
    auto raw = config->Raw();
    raw.transparent = mTransparent;
    if (!SetSurface(&raw)) {
      RenderThread::Get()->HandleWebRenderError(WebRenderError::NEW_SURFACE);
      return false;
    }
  }
#endif
  if (!mConfig || !mConfig->HasWindow()) {
    return false;
  }
  if (!wr_renderer_set_surface_paused(mRenderer, false)) {
    Fail();
    RenderThread::Get()->HandleWebRenderError(WebRenderError::NEW_SURFACE);
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
