/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <cstdlib>
#include <cstring>
#include <dlfcn.h>
#include <vector>

#ifdef MOZ_X11
#  define VK_USE_PLATFORM_XLIB_KHR
#endif
#include <vulkan/vulkan.h>

#include "VulkanWebRenderRequirements.h"
#include "mozilla/GfxInfoUtils.h"
#include "mozilla/ScopeExit.h"

#ifdef MOZ_X11
template <typename T, typename F>
static bool Enumerate(std::vector<T>& aValues, F aEnumerate) {
  uint32_t count = 0;
  if (aEnumerate(&count, nullptr) != VK_SUCCESS) {
    return false;
  }
  aValues.resize(count);
  if (aEnumerate(&count, aValues.data()) != VK_SUCCESS) {
    return false;
  }
  aValues.resize(count);
  return true;
}

static bool HasExtension(const std::vector<VkExtensionProperties>& aExtensions,
                         const char* aName) {
  for (const auto& extension : aExtensions) {
    if (!strcmp(extension.extensionName, aName)) {
      return true;
    }
  }
  return false;
}

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

static bool ProbeWebRender() {
  void* library = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
  if (!library) {
    library = dlopen("libvulkan.so", RTLD_NOW | RTLD_LOCAL);
  }
  if (!library) {
    record_error("Vulkan loader unavailable");
    return false;
  }
  auto closeLibrary = mozilla::MakeScopeExit([&] { dlclose(library); });
  auto getProc =
      cast<PFN_vkGetInstanceProcAddr>(dlsym(library, "vkGetInstanceProcAddr"));
  if (!getProc) {
    record_error("vkGetInstanceProcAddr unavailable");
    return false;
  }
  VkInstance instance = VK_NULL_HANDLE;
#  define LOAD_VK(name)                                                 \
    auto name = reinterpret_cast<PFN_##name>(getProc(instance, #name)); \
    if (!name) {                                                        \
      record_error("Missing Vulkan entry point: " #name);               \
      return false;                                                     \
    }
  LOAD_VK(vkEnumerateInstanceVersion);
  uint32_t version = 0;
  if (vkEnumerateInstanceVersion(&version) != VK_SUCCESS ||
      version < VK_API_VERSION_1_1) {
    record_error("Vulkan WebRender shaders require Vulkan 1.1");
    return false;
  }
  LOAD_VK(vkEnumerateInstanceExtensionProperties);
  std::vector<VkExtensionProperties> extensions;
  if (!Enumerate(extensions, [&](auto count, auto values) {
        return vkEnumerateInstanceExtensionProperties(nullptr, count, values);
      })) {
    record_error("Cannot enumerate Vulkan instance extensions");
    return false;
  }
  const char* instanceExtensions[] = {VK_KHR_SURFACE_EXTENSION_NAME,
                                      VK_KHR_XLIB_SURFACE_EXTENSION_NAME};
  for (const char* required : instanceExtensions) {
    if (!HasExtension(extensions, required)) {
      record_error("Missing Vulkan instance extension: %s", required);
      return false;
    }
  }
  LOAD_VK(vkCreateInstance);
  VkApplicationInfo app = {};
  app.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  app.pApplicationName = "Vulkan WebRender probe";
  app.apiVersion = VK_API_VERSION_1_1;
  VkInstanceCreateInfo info = {};
  info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  info.pApplicationInfo = &app;
  info.enabledExtensionCount = 2;
  info.ppEnabledExtensionNames = instanceExtensions;
  if (vkCreateInstance(&info, nullptr, &instance) != VK_SUCCESS) {
    record_error("Cannot create Vulkan 1.1 instance");
    return false;
  }
  LOAD_VK(vkDestroyInstance);
  auto destroyInstance =
      mozilla::MakeScopeExit([&] { vkDestroyInstance(instance, nullptr); });
  LOAD_VK(vkCreateXlibSurfaceKHR);
  LOAD_VK(vkDestroySurfaceKHR);
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
  VkXlibSurfaceCreateInfoKHR surfaceInfo = {};
  surfaceInfo.sType = VK_STRUCTURE_TYPE_XLIB_SURFACE_CREATE_INFO_KHR;
  surfaceInfo.dpy = display;
  surfaceInfo.window = window;
  VkSurfaceKHR surface = VK_NULL_HANDLE;
  if (vkCreateXlibSurfaceKHR(instance, &surfaceInfo, nullptr, &surface) !=
      VK_SUCCESS) {
    record_error("Cannot create Vulkan X11 surface");
    return false;
  }
  auto destroySurface = mozilla::MakeScopeExit(
      [&] { vkDestroySurfaceKHR(instance, surface, nullptr); });
  LOAD_VK(vkEnumeratePhysicalDevices);
  LOAD_VK(vkGetPhysicalDeviceProperties);
  LOAD_VK(vkGetPhysicalDeviceQueueFamilyProperties);
  LOAD_VK(vkGetPhysicalDeviceSurfaceSupportKHR);
  std::vector<VkPhysicalDevice> devices;
  if (!Enumerate(devices, [&](auto count, auto values) {
        return vkEnumeratePhysicalDevices(instance, count, values);
      })) {
    record_error("Cannot enumerate Vulkan adapters");
    return false;
  }
  VkPhysicalDevice selected = VK_NULL_HANDLE;
  VkPhysicalDeviceProperties selectedProperties = {};
  for (auto device : devices) {
    uint32_t count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(device, &count, nullptr);
    if (!count) {
      continue;
    }
    std::vector<VkQueueFamilyProperties> queues(count);
    vkGetPhysicalDeviceQueueFamilyProperties(device, &count, queues.data());
    VkBool32 present = VK_FALSE;
    if (!count || !queues[0].queueCount ||
        !(queues[0].queueFlags & VK_QUEUE_GRAPHICS_BIT) ||
        vkGetPhysicalDeviceSurfaceSupportKHR(device, 0, surface, &present) !=
            VK_SUCCESS ||
        !present) {
      continue;
    }
    VkPhysicalDeviceProperties properties = {};
    vkGetPhysicalDeviceProperties(device, &properties);
    // Match Device::select_adapter: device type, then adapter name.
    int rank = DeviceRank(properties.deviceType);
    int selectedRank = DeviceRank(selectedProperties.deviceType);
    if (!selected || rank < selectedRank ||
        (rank == selectedRank &&
         strcmp(properties.deviceName, selectedProperties.deviceName) < 0)) {
      selected = device;
      selectedProperties = properties;
    }
  }
  if (!selected) {
    record_error("No Vulkan graphics queue can present to X11");
    return false;
  }
  record_value("VULKAN_WEBRENDER_DEVICE\n%s\n", selectedProperties.deviceName);
  VulkanWebRenderRequirements requirements;
  requirements.device = selectedProperties;
  LOAD_VK(vkEnumerateDeviceExtensionProperties);
  if (!Enumerate(extensions, [&](auto count, auto values) {
        return vkEnumerateDeviceExtensionProperties(selected, nullptr, count,
                                                    values);
      })) {
    record_error("Cannot enumerate Vulkan device extensions");
    return false;
  }
  requirements.swapchain =
      HasExtension(extensions, VK_KHR_SWAPCHAIN_EXTENSION_NAME);
  LOAD_VK(vkGetPhysicalDeviceFormatProperties);
  VkFormatProperties formatProperties = {};
  vkGetPhysicalDeviceFormatProperties(selected, VK_FORMAT_R8G8B8A8_UNORM,
                                      &formatProperties);
  requirements.color = formatProperties.optimalTilingFeatures;
  vkGetPhysicalDeviceFormatProperties(selected, VK_FORMAT_D32_SFLOAT,
                                      &formatProperties);
  requirements.depth = formatProperties.optimalTilingFeatures;
  LOAD_VK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
  if (vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
          selected, surface, &requirements.surface) != VK_SUCCESS) {
    record_error("Cannot query Vulkan surface capabilities");
    return false;
  }
  LOAD_VK(vkGetPhysicalDeviceSurfaceFormatsKHR);
  std::vector<VkSurfaceFormatKHR> formats;
  if (!Enumerate(formats, [&](auto count, auto values) {
        return vkGetPhysicalDeviceSurfaceFormatsKHR(selected, surface, count,
                                                    values);
      })) {
    record_error("Cannot enumerate Vulkan surface formats");
    return false;
  }
  for (const auto& format : formats) {
    requirements.directFormat |=
        (format.format == VK_FORMAT_R8G8B8A8_UNORM ||
         format.format == VK_FORMAT_B8G8R8A8_UNORM) &&
        format.colorSpace == VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
  }
  LOAD_VK(vkGetPhysicalDeviceSurfacePresentModesKHR);
  std::vector<VkPresentModeKHR> modes;
  if (!Enumerate(modes, [&](auto count, auto values) {
        return vkGetPhysicalDeviceSurfacePresentModesKHR(selected, surface,
                                                         count, values);
      })) {
    record_error("Cannot enumerate Vulkan present modes");
    return false;
  }
  for (auto mode : modes) {
    requirements.fifo |= mode == VK_PRESENT_MODE_FIFO_KHR;
  }
  if (const char* failure = requirements.Failure()) {
    record_error("%s", failure);
    return false;
  }
  LOAD_VK(vkCreateDevice);
  LOAD_VK(vkDestroyDevice);
  float priority = 1.0f;
  VkDeviceQueueCreateInfo queueInfo = {};
  queueInfo.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
  queueInfo.queueCount = 1;
  queueInfo.pQueuePriorities = &priority;
  const char* deviceExtension = VK_KHR_SWAPCHAIN_EXTENSION_NAME;
  VkDeviceCreateInfo deviceInfo = {};
  deviceInfo.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
  deviceInfo.queueCreateInfoCount = 1;
  deviceInfo.pQueueCreateInfos = &queueInfo;
  deviceInfo.enabledExtensionCount = 1;
  deviceInfo.ppEnabledExtensionNames = &deviceExtension;
  VkDevice device = VK_NULL_HANDLE;
  if (vkCreateDevice(selected, &deviceInfo, nullptr, &device) != VK_SUCCESS) {
    record_error("Cannot create Vulkan WebRender device");
    return false;
  }
  vkDestroyDevice(device, nullptr);
  return true;
#  undef LOAD_VK
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
