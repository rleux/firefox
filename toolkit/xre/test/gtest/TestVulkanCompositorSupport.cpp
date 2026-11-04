/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "mozilla/webrender/VulkanCompositorSupport.h"
#include "gtest/gtest.h"

TEST(VulkanCompositorSupport, UnsupportedPlatformsCannotBeForced)
{
  using namespace mozilla::wr;
  for (auto platform :
       {VulkanCompositorPlatform::Win32, VulkanCompositorPlatform::Android,
        VulkanCompositorPlatform::Unsupported}) {
    for (bool x11Display : {false, true}) {
      for (bool requested : {false, true}) {
        for (bool software : {false, true}) {
          EXPECT_FALSE(CanUseVulkanCompositor(platform, x11Display, requested,
                                              software));
        }
      }
    }
  }
}

TEST(VulkanCompositorSupport, GtkRequiresRuntimeX11OptInAndHardware)
{
  using namespace mozilla::wr;
  for (bool x11Display : {false, true}) {
    for (bool requested : {false, true}) {
      for (bool software : {false, true}) {
        EXPECT_EQ(CanUseVulkanCompositor(VulkanCompositorPlatform::Gtk,
                                         x11Display, requested, software),
                  x11Display && requested && !software);
      }
    }
  }
}
