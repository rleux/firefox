/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanWindowPolicy.h"
#include "WidgetAccelerationPolicy.h"
#include "gtest/gtest.h"

TEST(VulkanWebRenderRequirements, WindowPolicyPreservesWorkingBackends)
{
  using namespace mozilla::widget;
  for (bool topLevel : {false, true}) {
    for (bool explicitAlpha : {false, true}) {
      EXPECT_EQ(SelectVulkanWindowPolicy(false, false, topLevel, explicitAlpha),
                VulkanWindowPolicy::ExistingVisual);
      EXPECT_EQ(SelectVulkanWindowPolicy(true, true, topLevel, explicitAlpha),
                VulkanWindowPolicy::ExistingVisual);
    }
  }
}

TEST(VulkanWebRenderRequirements, OpaqueFallbackKeepsTransparentWindowsSoftware)
{
  using namespace mozilla::widget;
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, true, false),
            VulkanWindowPolicy::OpaqueVisual);
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, true, true),
            VulkanWindowPolicy::Software);
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, false, false),
            VulkanWindowPolicy::Software);
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, false, true),
            VulkanWindowPolicy::Software);
}

TEST(VulkanWebRenderRequirements, SoftwareRequirementCannotBeForced)
{
  using namespace mozilla::widget;
  for (bool supports : {false, true}) {
    for (bool force : {false, true}) {
      EXPECT_FALSE(CanAccelerateWidget(supports, true, force));
      EXPECT_EQ(CanAccelerateWidget(supports, false, force), supports || force);
    }
  }
  const auto policy = SelectVulkanWindowPolicy(true, false, false, true);
  EXPECT_FALSE(
      CanAccelerateWidget(true, policy == VulkanWindowPolicy::Software, true));
}
