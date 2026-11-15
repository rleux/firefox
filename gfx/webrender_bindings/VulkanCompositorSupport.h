/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef mozilla_wr_VulkanCompositorSupport_h
#define mozilla_wr_VulkanCompositorSupport_h

namespace mozilla::wr {

enum class VulkanCompositorPlatform { Gtk, Win32, Android, Unsupported };

constexpr bool CanUseVulkanCompositor(VulkanCompositorPlatform aPlatform,
                                      bool aX11Display, bool aRequested,
                                      bool aSoftware) {
  // Gecko must provide both native lifetime retirement and CPU producer hosts.
  return aPlatform == VulkanCompositorPlatform::Gtk && aX11Display &&
         aRequested && !aSoftware;
}

constexpr VulkanCompositorPlatform BuildVulkanCompositorPlatform() {
#if defined(MOZ_WIDGET_GTK) && defined(MOZ_X11)
  return VulkanCompositorPlatform::Gtk;
#elif defined(XP_WIN)
  return VulkanCompositorPlatform::Win32;
#elif defined(MOZ_WIDGET_ANDROID)
  return VulkanCompositorPlatform::Android;
#else
  return VulkanCompositorPlatform::Unsupported;
#endif
}

// This shared admission decision also governs producer CPU fallback. Only the
// parent inspects GDK; worker and child processes read the replicated gfxVars.
bool IsVulkanCompositorRequested();

}  // namespace mozilla::wr

#endif
