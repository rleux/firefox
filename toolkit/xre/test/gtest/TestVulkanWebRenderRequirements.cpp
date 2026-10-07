/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanWebRenderRequirements.h"
#include "gtest/gtest.h"

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
