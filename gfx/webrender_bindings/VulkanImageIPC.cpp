/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanImageIPC.h"

#include <algorithm>

#include "mozilla/CheckedInt.h"
#include "mozilla/StaticMutex.h"
#include "mozilla/StaticPtr.h"
#include "mozilla/layers/VulkanImages.h"
#include "mozilla/webrender/RenderTextureHost.h"
#include "mozilla/webrender/RenderThread.h"
#include "nsTArray.h"
#if defined(XP_LINUX) && !defined(ANDROID)
#  include "RenderVulkanDMABufTextureHost.h"
#endif

namespace mozilla::wr {

static StaticMutex sCapabilitiesMutex;
static StaticAutoPtr<nsTArray<VulkanImageCapabilities*>> sCapabilities
    MOZ_GUARDED_BY(sCapabilitiesMutex);

UniquePtr<VulkanImageCapabilities> VulkanImageCapabilities::Register(
    WrVulkanExternalImages* aImages) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (!aImages) {
    return nullptr;
  }
  auto* capabilities = wr_vulkan_dmabuf_capabilities_new(aImages);
  return UniquePtr<VulkanImageCapabilities>(
      new VulkanImageCapabilities(capabilities));
}

VulkanImageCapabilities::VulkanImageCapabilities(
    WrVulkanDmaBufCapabilities* aCapabilities)
    : mCapabilities(aCapabilities) {
  StaticMutexAutoLock lock(sCapabilitiesMutex);
  if (!sCapabilities) {
    sCapabilities = new nsTArray<VulkanImageCapabilities*>();
  }
  sCapabilities->AppendElement(this);
}

VulkanImageCapabilities::~VulkanImageCapabilities() {
  StaticMutexAutoLock lock(sCapabilitiesMutex);
  MOZ_RELEASE_ASSERT(sCapabilities && sCapabilities->RemoveElement(this));
  if (sCapabilities->IsEmpty()) {
    sCapabilities = nullptr;
  }
}

void VulkanImageCapabilities::Deleter::operator()(
    WrVulkanDmaBufCapabilities* aCapabilities) const {
  wr_vulkan_dmabuf_capabilities_delete(aCapabilities);
}

bool VulkanImageCapabilities::Supports(const WrVulkanDmaBufDescriptor& aImage) {
  StaticMutexAutoLock lock(sCapabilitiesMutex);
  if (!sCapabilities || sCapabilities->IsEmpty()) {
    return false;
  }
  for (const auto* capabilities : *sCapabilities) {
    if (!capabilities->mCapabilities ||
        !wr_vulkan_dmabuf_capabilities_supports(
            capabilities->mCapabilities.get(), &aImage)) {
      return false;
    }
  }
  return true;
}

bool VulkanImageCapabilities::SupportsForeignRGB(
    const WrVulkanForeignRgbDescriptor& aImage, uint64_t aDrmMajor,
    uint64_t aDrmMinor) {
  StaticMutexAutoLock lock(sCapabilitiesMutex);
  if (!sCapabilities || sCapabilities->IsEmpty()) {
    return false;
  }
  for (const auto* capabilities : *sCapabilities) {
    if (!capabilities->mCapabilities ||
        !wr_vulkan_dmabuf_capabilities_supports_foreign_rgb(
            capabilities->mCapabilities.get(), &aImage, aDrmMajor, aDrmMinor)) {
      return false;
    }
  }
  return true;
}

static bool ValidHandle(gfx::FileHandleWrapper* aHandle) {
  return aHandle && FileHandleIsValid(aHandle->GetHandle());
}

static bool ValidTimeline(const layers::VulkanTimelineDescriptor& aTimeline) {
  return ValidHandle(aTimeline.handle()) && aTimeline.value();
}

bool ValidateVulkanImagePublication(
    const layers::VulkanImagePublication& aImage) {
  const auto size = aImage.size();
  if (!aImage.publicationId() || !ValidHandle(aImage.memory()) ||
      !ValidTimeline(aImage.ready()) || size.width <= 0 || size.height <= 0 ||
      !aImage.stride() ||
      (aImage.format() != gfx::SurfaceFormat::R8G8B8A8 &&
       aImage.format() != gfx::SurfaceFormat::B8G8R8A8) ||
      !(CheckedInt<size_t>(size.width) * size.height * 4).isValid()) {
    return false;
  }
  if (!aImage.modifier()) {
    auto rowBytes = CheckedInt<uint64_t>(size.width) * 4;
    auto end = CheckedInt<uint64_t>(aImage.stride()) * (size.height - 1) +
               aImage.offset() + rowBytes;
    if (!end.isValid() || aImage.stride() < rowBytes.value() ||
        aImage.stride() % 4 || aImage.offset() % 4) {
      return false;
    }
  }
  return true;
}

bool ValidateVulkanImageReturn(const layers::VulkanImageReturnMessage& aReturn,
                               const layers::VulkanImagePublication& aImage) {
  if (!aImage.publicationId() ||
      aReturn.publicationId() != aImage.publicationId()) {
    return false;
  }
  switch (aReturn.status()) {
    case VulkanImageReturnStatus::Unused:
    case VulkanImageReturnStatus::Abandoned:
      return aReturn.signal().isNothing();
    case VulkanImageReturnStatus::Submitted:
      return aReturn.signal() && ValidTimeline(aReturn.signal().ref()) &&
             aReturn.signal()->deviceUUID() == aImage.ready().deviceUUID() &&
             aReturn.signal()->driverUUID() == aImage.ready().driverUUID();
  }
  return false;
}

already_AddRefed<RenderTextureHost> CreateVulkanImageHost(
    const layers::VulkanImagePublication& aImage,
    std::function<void(layers::VulkanImageReturnMessage&&)>&& aReturn) {
#if defined(XP_LINUX) && !defined(ANDROID)
  if (!aReturn || !ValidateVulkanImagePublication(aImage)) {
    return nullptr;
  }
  WrVulkanDmaBufDescriptor image{};
  image.fd = aImage.memory()->GetHandle();
  image.width = aImage.size().width;
  image.height = aImage.size().height;
  image.format = aImage.format() == gfx::SurfaceFormat::R8G8B8A8
                     ? ImageFormat::RGBA8
                     : ImageFormat::BGRA8;
  image.modifier = aImage.modifier();
  image.offset = aImage.offset();
  image.stride = aImage.stride();
  image.copy_src = aImage.copySrc();
  image.copy_dst = aImage.copyDst();
  image.color_target = aImage.colorTarget();
  WrVulkanTimelineDescriptor ready{};
  ready.fd = aImage.ready().handle()->GetHandle();
  const auto& device = aImage.ready().deviceUUID();
  const auto& driver = aImage.ready().driverUUID();
  std::copy(device.begin(), device.end(), image.device_uuid);
  std::copy(driver.begin(), driver.end(), image.driver_uuid);
  std::copy(device.begin(), device.end(), ready.device_uuid);
  std::copy(driver.begin(), driver.end(), ready.driver_uuid);
  RefPtr<RenderTextureHost> host = RenderVulkanDMABufTextureHost::Create(
      image, ready, aImage.ready().value(),
      [id = aImage.publicationId(),
       callback = std::move(aReturn)](VulkanImageReturn&& aResult) {
        auto status = aResult.mStatus;
        Maybe<layers::VulkanTimelineDescriptor> signal;
        if (status == VulkanImageReturnStatus::Submitted) {
          if (aResult.mSemaphore && aResult.mValue) {
            RefPtr<gfx::FileHandleWrapper> handle =
                new gfx::FileHandleWrapper(std::move(aResult.mSemaphore));
            signal.emplace(handle, aResult.mDeviceUUID, aResult.mDriverUUID,
                           aResult.mValue);
          } else {
            status = VulkanImageReturnStatus::Abandoned;
          }
        }
        callback(
            layers::VulkanImageReturnMessage(id, status, std::move(signal)));
      });
  return host.forget();
#else
  return nullptr;
#endif
}

}  // namespace mozilla::wr
