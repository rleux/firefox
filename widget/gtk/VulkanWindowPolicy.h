/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef widget_gtk_VulkanWindowPolicy_h
#define widget_gtk_VulkanWindowPolicy_h

namespace mozilla::widget {

enum class VulkanWindowPolicy { ExistingVisual, OpaqueVisual, Software };

constexpr VulkanWindowPolicy SelectVulkanWindowPolicy(bool aUseVulkan,
                                                      bool aSupportsAlpha,
                                                      bool aIsTopLevel,
                                                      bool aExplicitAlpha) {
  if (!aUseVulkan || aSupportsAlpha) {
    return VulkanWindowPolicy::ExistingVisual;
  }
  return aIsTopLevel && !aExplicitAlpha ? VulkanWindowPolicy::OpaqueVisual
                                        : VulkanWindowPolicy::Software;
}

}  // namespace mozilla::widget

#endif
