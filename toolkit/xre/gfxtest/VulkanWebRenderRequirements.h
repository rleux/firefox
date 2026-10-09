/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef VulkanWebRenderRequirements_h
#define VulkanWebRenderRequirements_h

#include <vulkan/vulkan.h>

struct VulkanWebRenderRequirements {
  static int DeviceRank(VkPhysicalDeviceType aType) {
    switch (aType) {
      case VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU:
        return 0;
      case VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU:
        return 1;
      case VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU:
        return 2;
      case VK_PHYSICAL_DEVICE_TYPE_CPU:
        return 3;
      default:
        return 4;
    }
  }

  static bool PreferDevice(const VkPhysicalDeviceProperties& aCandidate,
                           const VkPhysicalDeviceProperties& aSelected) {
    return DeviceRank(aCandidate.deviceType) < DeviceRank(aSelected.deviceType);
  }

  static bool SupportsAlpha(VkCompositeAlphaFlagsKHR aModes) {
    return aModes & (VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR |
                     VK_COMPOSITE_ALPHA_INHERIT_BIT_KHR);
  }

  VkPhysicalDeviceProperties device = {};
  VkFormatFeatureFlags color = 0;
  VkFormatFeatureFlags depth = 0;
  VkSurfaceCapabilitiesKHR surface = {};
  bool swapchain = false;
  bool directFormat = false;
  bool fifo = false;

  const char* Failure(bool aTransparent = false) const {
    if (device.apiVersion < VK_API_VERSION_1_1 ||
        device.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU) {
      return "Vulkan WebRender requires a hardware Vulkan 1.1 adapter";
    }
    if (!swapchain) {
      return "Vulkan adapter lacks VK_KHR_swapchain";
    }
    const auto colorFlags = VK_FORMAT_FEATURE_TRANSFER_SRC_BIT |
                            VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT;
    const auto depthFlags = VK_FORMAT_FEATURE_TRANSFER_SRC_BIT |
                            VK_FORMAT_FEATURE_DEPTH_STENCIL_ATTACHMENT_BIT;
    if ((color & colorFlags) != colorFlags ||
        (depth & depthFlags) != depthFlags) {
      return "Vulkan adapter lacks required texture format usages";
    }
    const bool alphaSupported =
        aTransparent ? SupportsAlpha(surface.supportedCompositeAlpha)
                     : (surface.supportedCompositeAlpha &
                        VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR);
    if (!(surface.supportedUsageFlags & VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT) ||
        !alphaSupported) {
      return aTransparent
                 ? "Vulkan surface lacks direct alpha rendering support"
                 : "Vulkan surface lacks direct opaque rendering support";
    }
    if (!directFormat || !fifo) {
      return "Vulkan surface lacks UNORM/sRGB or FIFO presentation";
    }
    if (surface.currentExtent.width != UINT32_MAX &&
        surface.currentExtent.height != UINT32_MAX &&
        (!surface.currentExtent.width || !surface.currentExtent.height ||
         surface.currentExtent.width > device.limits.maxImageDimension2D ||
         surface.currentExtent.height > device.limits.maxImageDimension2D)) {
      return "Vulkan surface extent is unavailable or exceeds device limits";
    }
    return nullptr;
  }
};

#endif
