/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanWebRenderRequirements.h"
#include "VulkanWindowPolicy.h"
#include "WidgetAccelerationPolicy.h"
#include <cstring>
#include "gtest/gtest.h"

TEST(VulkanWebRenderRequirements, AlphaSupportPreservesCompatibleModes)
{
  for (auto modes : {VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR,
                     VK_COMPOSITE_ALPHA_INHERIT_BIT_KHR}) {
    EXPECT_TRUE(VulkanWebRenderRequirements::SupportsAlpha(modes));
    EXPECT_TRUE(VulkanWebRenderRequirements::SupportsAlpha(
        modes | VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR));
  }
  EXPECT_FALSE(VulkanWebRenderRequirements::SupportsAlpha(0));
  EXPECT_FALSE(VulkanWebRenderRequirements::SupportsAlpha(
      VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR));
  EXPECT_FALSE(VulkanWebRenderRequirements::SupportsAlpha(
      VK_COMPOSITE_ALPHA_POST_MULTIPLIED_BIT_KHR));
}

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

static VulkanWebRenderRequirements SupportedVulkanDevice() {
  VulkanWebRenderRequirements requirements;
  requirements.device.apiVersion = VK_API_VERSION_1_1;
  requirements.device.deviceType = VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU;
  requirements.device.limits.maxImageDimension2D = 4096;
  requirements.color = VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT |
                       VK_FORMAT_FEATURE_TRANSFER_SRC_BIT;
  requirements.depth = VK_FORMAT_FEATURE_DEPTH_STENCIL_ATTACHMENT_BIT |
                       VK_FORMAT_FEATURE_TRANSFER_SRC_BIT;
  requirements.surface.supportedUsageFlags =
      VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
  requirements.surface.supportedCompositeAlpha =
      VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
  requirements.surface.currentExtent = {16, 16};
  requirements.swapchain = true;
  requirements.directFormat = true;
  requirements.fifo = true;
  return requirements;
}

TEST(VulkanWebRenderRequirements, BaselineWithoutOptionalFeatures)
{
  auto requirements = SupportedVulkanDevice();
  EXPECT_EQ(requirements.Failure(), nullptr);
  requirements.surface.currentExtent = {UINT32_MAX, UINT32_MAX};
  EXPECT_EQ(requirements.Failure(), nullptr);
}

TEST(VulkanWebRenderRequirements, RejectOldApiAndSoftwareAdapters)
{
  auto requirements = SupportedVulkanDevice();
  requirements.device.apiVersion = VK_API_VERSION_1_0;
  EXPECT_NE(requirements.Failure(), nullptr);
  requirements.device.apiVersion = VK_API_VERSION_1_3;
  requirements.device.deviceType = VK_PHYSICAL_DEVICE_TYPE_CPU;
  EXPECT_NE(requirements.Failure(), nullptr);
}

TEST(VulkanWebRenderRequirements, RejectMissingSwapchain)
{
  auto requirements = SupportedVulkanDevice();
  requirements.swapchain = false;
  EXPECT_NE(requirements.Failure(), nullptr);
}

TEST(VulkanWebRenderRequirements, RejectMissingTextureUsages)
{
  for (auto usage : {VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT,
                     VK_FORMAT_FEATURE_TRANSFER_SRC_BIT}) {
    auto requirements = SupportedVulkanDevice();
    requirements.color &= ~usage;
    EXPECT_NE(requirements.Failure(), nullptr);
  }
  for (auto usage : {VK_FORMAT_FEATURE_DEPTH_STENCIL_ATTACHMENT_BIT,
                     VK_FORMAT_FEATURE_TRANSFER_SRC_BIT}) {
    auto requirements = SupportedVulkanDevice();
    requirements.depth &= ~usage;
    EXPECT_NE(requirements.Failure(), nullptr);
  }
}

TEST(VulkanWebRenderRequirements, RejectUnusableSurface)
{
  auto requirements = SupportedVulkanDevice();
  requirements.surface.supportedUsageFlags = VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
  EXPECT_NE(requirements.Failure(), nullptr);
  requirements = SupportedVulkanDevice();
  requirements.surface.supportedCompositeAlpha =
      VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR;
  EXPECT_NE(requirements.Failure(), nullptr);
  requirements = SupportedVulkanDevice();
  requirements.directFormat = false;
  EXPECT_NE(requirements.Failure(), nullptr);
  requirements = SupportedVulkanDevice();
  requirements.fifo = false;
  EXPECT_NE(requirements.Failure(), nullptr);
  for (auto extent : {VkExtent2D{0, 16}, VkExtent2D{16, 0},
                      VkExtent2D{4097, 16}, VkExtent2D{16, 4097}}) {
    requirements = SupportedVulkanDevice();
    requirements.surface.currentExtent = extent;
    EXPECT_NE(requirements.Failure(), nullptr);
  }
}

TEST(VulkanWebRenderRequirements, AdapterSelectionPreservesEnumerationOrder)
{
  VkPhysicalDeviceProperties selected = {};
  selected.deviceType = VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU;
  std::strcpy(selected.deviceName, "Z adapter");
  VkPhysicalDeviceProperties candidate = selected;
  std::strcpy(candidate.deviceName, "A adapter");
  EXPECT_FALSE(VulkanWebRenderRequirements::PreferDevice(candidate, selected));
  EXPECT_FALSE(VulkanWebRenderRequirements::PreferDevice(selected, candidate));
  selected.deviceType = VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU;
  EXPECT_TRUE(VulkanWebRenderRequirements::PreferDevice(candidate, selected));
  EXPECT_FALSE(VulkanWebRenderRequirements::PreferDevice(selected, candidate));
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

TEST(VulkanWebRenderRequirements, AlphaVisualRequiresUsableSurface)
{
  auto requirements = SupportedVulkanDevice();
  requirements.surface.supportedCompositeAlpha =
      VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR;
  EXPECT_EQ(requirements.Failure(true), nullptr);
  EXPECT_NE(requirements.Failure(), nullptr);
  requirements.directFormat = false;
  EXPECT_NE(requirements.Failure(true), nullptr);
  requirements.directFormat = true;
  requirements.fifo = false;
  EXPECT_NE(requirements.Failure(true), nullptr);
}
