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
static bool ProbeWebRender() {
  using namespace mozilla::gfx;
  WrStartupProbeReport report{};
  auto* probe = wr_startup_probe_new(&report);
  if (!probe) {
    record_error("%s", report.error);
    return false;
  }
  auto deleteProbe =
      mozilla::MakeScopeExit([&] { wr_startup_probe_delete(probe); });
  Display* display = XOpenDisplay(nullptr);
  if (!display) {
    record_error("Cannot open X11 display for Vulkan WebRender");
    return false;
  }
  auto closeDisplay = mozilla::MakeScopeExit([&] { XCloseDisplay(display); });
  Window window = XCreateSimpleWindow(display, DefaultRootWindow(display), 0, 0,
                                      16, 16, 0, 0, 0);
  auto destroyWindow =
      mozilla::MakeScopeExit([&] { XDestroyWindow(display, window); });
  const bool selected = wr_startup_probe_select_xlib(
      probe, display, uint64_t(window), DefaultScreen(display), &report);
  if (report.has_device) {
    record_value("VULKAN_WEBRENDER_DEVICE\n%s\n", report.device_name);
  }
  if (!selected) {
    record_error("%s", report.error);
    return false;
  }
  if (!wr_startup_probe_finish(probe, &report)) {
    record_error("%s", report.error);
    return false;
  }
  return true;
}
#endif

int vulkanwrtest() {
  const char* debug = getenv("MOZ_GFX_DEBUG");
  enable_logging = debug && *debug == '1';
  if (!enable_logging) {
    close_logging();
  }
  log("Vulkan WebRender capability probe start\n");
#ifdef MOZ_X11
  bool supported = ProbeWebRender();
#else
  bool supported = false;
  record_error("Vulkan WebRender probe requires X11");
#endif
  record_value("VULKAN_WEBRENDER\n%s\n", supported ? "TRUE" : "FALSE");
  record_flush();
  return EXIT_SUCCESS;
}
