/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <cinttypes>
#include <cstdlib>

#ifdef MOZ_X11
#  include <X11/Xutil.h>
#  include "mozilla/gfx/wr_startup_probe_ffi_generated.h"
#endif

#include "mozilla/GfxInfoUtils.h"
#include "mozilla/ScopeExit.h"

#ifdef MOZ_X11
static bool ProbeWebRender(bool aShared, bool aHalLabels, bool aRequireHardware,
                           const char* aBackends) {
  using namespace mozilla::gfx;
  WrStartupProbeReport report{};
  const auto fail = [aShared](const char* reason) {
    if (aShared) {
      record_value("VULKAN_WEBRENDER_SHARED_REASON\n%s\n", reason);
    } else {
      record_error("%s", reason);
    }
    return false;
  };
  auto* probe = aShared ? wr_startup_probe_new_shared(
                              aHalLabels, aRequireHardware, aBackends, &report)
                        : wr_startup_probe_new(&report);
  if (!probe) {
    return fail(report.error);
  }
  auto deleteProbe =
      mozilla::MakeScopeExit([&] { wr_startup_probe_delete(probe); });
  Display* display = XOpenDisplay(nullptr);
  if (!display) {
    return fail("Cannot open X11 display for Vulkan WebRender");
  }
  auto closeDisplay = mozilla::MakeScopeExit([&] { XCloseDisplay(display); });
  Window window = XCreateSimpleWindow(display, DefaultRootWindow(display), 0, 0,
                                      16, 16, 0, 0, 0);
  auto destroyWindow =
      mozilla::MakeScopeExit([&] { XDestroyWindow(display, window); });
  const bool selected = wr_startup_probe_select_xlib(
      probe, display, uint64_t(window), DefaultScreen(display), &report);
  if (report.has_device) {
    record_value(
        "%s\n%s\n",
        aShared ? "VULKAN_WEBRENDER_SHARED_DEVICE" : "VULKAN_WEBRENDER_DEVICE",
        report.device_name);
  }
  if (!selected) {
    return fail(report.error);
  }
  for (int screen = 0; screen < ScreenCount(display); ++screen) {
    XVisualInfo query = {};
    query.screen = screen;
    query.depth = 32;
    query.c_class = TrueColor;
    int count = 0;
    XVisualInfo* visuals = XGetVisualInfo(
        display, VisualScreenMask | VisualDepthMask | VisualClassMask, &query,
        &count);
    if (!visuals) {
      continue;
    }
    auto freeVisuals = mozilla::MakeScopeExit([&] { XFree(visuals); });
    for (int i = 0; i < count; ++i) {
      const auto& visual = visuals[i];
      XSetWindowAttributes attributes = {};
      attributes.colormap = XCreateColormap(
          display, RootWindow(display, screen), visual.visual, AllocNone);
      auto freeColormap = mozilla::MakeScopeExit(
          [&] { XFreeColormap(display, attributes.colormap); });
      Window alphaWindow = XCreateWindow(
          display, RootWindow(display, screen), 0, 0, 16, 16, 0, visual.depth,
          InputOutput, visual.visual, CWColormap | CWBorderPixel, &attributes);
      auto destroyAlphaWindow =
          mozilla::MakeScopeExit([&] { XDestroyWindow(display, alphaWindow); });
      if (wr_startup_probe_alpha_xlib(probe, display, uint64_t(alphaWindow),
                                      screen)) {
        uint64_t key = (uint64_t(screen) << 32) | visual.visualid;
        record_value("%s\n%" PRIu64 "\n",
                     aShared ? "VULKAN_WEBRENDER_SHARED_ALPHA_VISUAL"
                             : "VULKAN_WEBRENDER_ALPHA_VISUAL",
                     key);
      }
    }
  }
  if (!wr_startup_probe_finish(probe, &report)) {
    return fail(report.error);
  }
  return true;
}
#endif

int vulkanwrtest(bool aShared, bool aHalLabels, bool aRequireHardware,
                 const char* aBackends) {
  const char* debug = getenv("MOZ_GFX_DEBUG");
  enable_logging = debug && *debug == '1';
  if (!enable_logging) {
    close_logging();
  }
  log("Vulkan %s capability probe start\n",
      aShared ? "shared WebRender/WebGPU" : "WebRender");
#ifdef MOZ_X11
  bool supported =
      ProbeWebRender(aShared, aHalLabels, aRequireHardware, aBackends);
#else
  bool supported = false;
  if (aShared) {
    record_value(
        "VULKAN_WEBRENDER_SHARED_REASON\nVulkan shared probe requires X11\n");
  } else {
    record_error("Vulkan WebRender probe requires X11");
  }
#endif
  record_value("%s\n%s\n",
               aShared ? "VULKAN_WEBRENDER_SHARED" : "VULKAN_WEBRENDER",
               supported ? "TRUE" : "FALSE");
  record_flush();
  return EXIT_SUCCESS;
}
