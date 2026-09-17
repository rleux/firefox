/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <vulkan/vulkan.h>

#include <charconv>
#include <cinttypes>
#include <cstdio>
#include <cstring>
#include <string_view>
#include <vector>

namespace {

struct Instance {
  VkInstance mHandle = VK_NULL_HANDLE;

  ~Instance() {
    if (mHandle) {
      vkDestroyInstance(mHandle, nullptr);
    }
  }
};

template <typename T>
bool ParseNumber(const char* aText, T& aValue) {
  std::string_view text(aText);
  int base = 10;
  if (text.substr(0, 2) == "0x") {
    text.remove_prefix(2);
    base = 16;
  }
  const auto result =
      std::from_chars(text.data(), text.data() + text.size(), aValue, base);
  return result.ec == std::errc() && result.ptr == text.data() + text.size();
}

bool Check(VkResult aResult, const char* aOperation) {
  if (aResult != VK_SUCCESS) {
    std::fprintf(stderr, "%s failed: VkResult %d\n", aOperation, aResult);
    return false;
  }
  return true;
}

bool HasExtension(const std::vector<VkExtensionProperties>& aExtensions,
                  const char* aName) {
  for (const auto& extension : aExtensions) {
    if (!std::strcmp(extension.extensionName, aName)) {
      return true;
    }
  }
  return false;
}

bool QueryFormat(VkPhysicalDevice aDevice, VkFormat aFormat, const char* aName,
                 VkImageCreateFlags aFlags, uint64_t aModifier, uint32_t aWidth,
                 uint32_t aHeight,
                 VkImageUsageFlags aUsage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT) {
  VkDrmFormatModifierPropertiesListEXT modifiers{};
  modifiers.sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT;
  VkFormatProperties2 formatProperties{};
  formatProperties.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2;
  formatProperties.pNext = &modifiers;
  vkGetPhysicalDeviceFormatProperties2(aDevice, aFormat, &formatProperties);
  std::vector<VkDrmFormatModifierPropertiesEXT> properties(
      modifiers.drmFormatModifierCount);
  modifiers.pDrmFormatModifierProperties = properties.data();
  vkGetPhysicalDeviceFormatProperties2(aDevice, aFormat, &formatProperties);
  if (modifiers.drmFormatModifierCount > properties.size()) {
    std::fprintf(stderr, "Modifier list changed during query\n");
    return false;
  }
  properties.resize(modifiers.drmFormatModifierCount);

  std::printf("\n%s, %ux%u, flags=0x%x, usage=0x%x\n", aName, aWidth, aHeight,
              aFlags, aUsage);
  for (const auto& property : properties) {
    if (property.drmFormatModifier == aModifier) {
      std::printf("  modifier-memory-planes=%u, format-features=0x%x\n",
                  property.drmFormatModifierPlaneCount,
                  property.drmFormatModifierTilingFeatures);
    }
  }

  VkPhysicalDeviceExternalImageFormatInfo externalInfo{};
  externalInfo.sType =
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO;
  externalInfo.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
  VkPhysicalDeviceImageDrmFormatModifierInfoEXT modifierInfo{};
  modifierInfo.sType =
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT;
  modifierInfo.pNext = &externalInfo;
  modifierInfo.drmFormatModifier = aModifier;
  modifierInfo.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
  const VkFormat viewFormats[] = {VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
                                  VK_FORMAT_R8_UNORM, VK_FORMAT_R8G8_UNORM};
  VkImageFormatListCreateInfo viewInfo{};
  viewInfo.sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO;
  viewInfo.viewFormatCount = 3;
  viewInfo.pViewFormats = viewFormats;
  if (aFlags & VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT) {
    externalInfo.pNext = &viewInfo;
  }
  VkPhysicalDeviceImageFormatInfo2 imageInfo{};
  imageInfo.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2;
  imageInfo.pNext = &modifierInfo;
  imageInfo.format = aFormat;
  imageInfo.type = VK_IMAGE_TYPE_2D;
  imageInfo.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;
  imageInfo.usage = aUsage;
  imageInfo.flags = aFlags;

  VkExternalImageFormatProperties externalProperties{};
  externalProperties.sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES;
  VkImageFormatProperties2 imageProperties{};
  imageProperties.sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2;
  imageProperties.pNext = &externalProperties;
  const auto result = vkGetPhysicalDeviceImageFormatProperties2(
      aDevice, &imageInfo, &imageProperties);
  if (result == VK_ERROR_FORMAT_NOT_SUPPORTED) {
    std::printf(
        "  unsupported format/modifier/usage/flags/handle combination\n");
    return true;
  }
  if (!Check(result, "vkGetPhysicalDeviceImageFormatProperties2")) {
    return false;
  }
  const auto& memory = externalProperties.externalMemoryProperties;
  const auto& limits = imageProperties.imageFormatProperties;
  std::printf("  external-memory-features=0x%x, compatible-handle-types=0x%x\n",
              memory.externalMemoryFeatures, memory.compatibleHandleTypes);
  std::printf("  max-extent=%ux%ux%u, max-resource-size=%" PRIu64 "\n",
              limits.maxExtent.width, limits.maxExtent.height,
              limits.maxExtent.depth, limits.maxResourceSize);
  const bool candidate =
      (memory.externalMemoryFeatures &
       VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) &&
      (memory.compatibleHandleTypes &
       VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT) &&
      (limits.sampleCounts & VK_SAMPLE_COUNT_1_BIT) &&
      limits.maxMipLevels >= 1 && limits.maxArrayLayers >= 1 &&
      aWidth <= limits.maxExtent.width && aHeight <= limits.maxExtent.height;
  std::printf("  import-query-candidate=%s, dedicated-only=%s\n",
              candidate ? "yes" : "no",
              memory.externalMemoryFeatures &
                      VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT
                  ? "yes"
                  : "no");
  return true;
}

}  // namespace

int main(int argc, char** argv) {
  uint64_t modifier;
  uint32_t width, height;
  if (argc != 5 || !ParseNumber(argv[2], modifier) ||
      !ParseNumber(argv[3], width) || !ParseNumber(argv[4], height) || !width ||
      !height || width % 2 || height % 2) {
    std::fprintf(stderr,
                 "Usage: %s DRM_RENDER_NODE MODIFIER EVEN_WIDTH EVEN_HEIGHT\n",
                 argv[0]);
    return 1;
  }
  struct stat node{};
  if (stat(argv[1], &node) != 0 || !S_ISCHR(node.st_mode)) {
    std::fprintf(stderr, "Not an accessible DRM character device: %s\n",
                 argv[1]);
    return 1;
  }

  VkApplicationInfo application{};
  application.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  application.pApplicationName = "WebRender video DMA-BUF capability probe";
  application.apiVersion = VK_API_VERSION_1_1;
  VkInstanceCreateInfo create{};
  create.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  create.pApplicationInfo = &application;
  Instance instance;
  if (!Check(vkCreateInstance(&create, nullptr, &instance.mHandle),
             "vkCreateInstance")) {
    return 1;
  }
  uint32_t count = 0;
  if (!Check(vkEnumeratePhysicalDevices(instance.mHandle, &count, nullptr),
             "vkEnumeratePhysicalDevices")) {
    return 1;
  }
  if (!count) {
    std::fprintf(stderr, "No Vulkan physical devices\n");
    return 2;
  }
  std::vector<VkPhysicalDevice> devices(count);
  if (!Check(
          vkEnumeratePhysicalDevices(instance.mHandle, &count, devices.data()),
          "vkEnumeratePhysicalDevices")) {
    return 1;
  }
  devices.resize(count);

  for (auto device : devices) {
    uint32_t extensionCount = 0;
    if (!Check(vkEnumerateDeviceExtensionProperties(device, nullptr,
                                                    &extensionCount, nullptr),
               "vkEnumerateDeviceExtensionProperties")) {
      return 1;
    }
    if (!extensionCount) {
      continue;
    }
    std::vector<VkExtensionProperties> extensions(extensionCount);
    if (!Check(vkEnumerateDeviceExtensionProperties(
                   device, nullptr, &extensionCount, extensions.data()),
               "vkEnumerateDeviceExtensionProperties")) {
      return 1;
    }
    extensions.resize(extensionCount);
    if (!HasExtension(extensions, VK_EXT_PHYSICAL_DEVICE_DRM_EXTENSION_NAME)) {
      continue;
    }
    VkPhysicalDeviceDrmPropertiesEXT drm{};
    drm.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT;
    VkPhysicalDeviceProperties2 properties{};
    properties.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
    properties.pNext = &drm;
    vkGetPhysicalDeviceProperties2(device, &properties);
    if (!drm.hasRender || drm.renderMajor != major(node.st_rdev) ||
        drm.renderMinor != minor(node.st_rdev)) {
      continue;
    }
    std::printf("Device: %s (vendor=0x%x, device=0x%x), DRM %u:%u\n",
                properties.properties.deviceName,
                properties.properties.vendorID, properties.properties.deviceID,
                major(node.st_rdev), minor(node.st_rdev));
    std::printf("Modifier: 0x%" PRIx64 "\n", modifier);
    const char* required[] = {VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
                              VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
                              VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
                              VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME};
    bool available = true;
    for (const auto* name : required) {
      const bool supported = HasExtension(extensions, name);
      std::printf("%s: %s\n", name, supported ? "yes" : "no");
      available &= supported;
    }
    if (!available) {
      std::printf("Required VA-API import extensions unavailable\n");
      return 2;
    }
    if (!QueryFormat(device, VK_FORMAT_G8_B8R8_2PLANE_420_UNORM, "NV12", 0,
                     modifier, width, height) ||
        !QueryFormat(device, VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
                     "NV12 direct plane views",
                     VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT, modifier, width,
                     height, VK_IMAGE_USAGE_SAMPLED_BIT) ||
        !QueryFormat(
            device, VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
            "NV12 direct plane views with readback",
            VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT, modifier, width, height,
            VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT) ||
        !QueryFormat(device, VK_FORMAT_R8_UNORM, "Y layer (alias candidate)",
                     VK_IMAGE_CREATE_ALIAS_BIT, modifier, width, height) ||
        !QueryFormat(device, VK_FORMAT_R8G8_UNORM, "UV layer (alias candidate)",
                     VK_IMAGE_CREATE_ALIAS_BIT, modifier, width / 2,
                     height / 2)) {
      return 1;
    }
    std::printf("\nQueries only: no memory imported or GPU work submitted.\n");
    return 0;
  }
  std::fprintf(stderr, "No Vulkan device matched the DRM render node\n");
  return 2;
}
