/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_VULKANIMAGEIPC_H
#define MOZILLA_GFX_VULKANIMAGEIPC_H

#include <functional>

#include "ipc/EnumSerializer.h"
#include "mozilla/AlreadyAddRefed.h"
#include "mozilla/UniquePtr.h"
#include "mozilla/webrender/VulkanImageTypes.h"

namespace IPC {
template <>
struct ParamTraits<mozilla::wr::VulkanImageReturnStatus>
    : ContiguousEnumSerializerInclusive<
          mozilla::wr::VulkanImageReturnStatus,
          mozilla::wr::VulkanImageReturnStatus::Unused,
          mozilla::wr::VulkanImageReturnStatus::Abandoned> {};
}  // namespace IPC

namespace mozilla::layers {
class VulkanImagePublication;
class VulkanImageReturnMessage;
}  // namespace mozilla::layers

namespace mozilla::wr {
class RenderTextureHost;
struct WrVulkanExternalImages;
struct WrVulkanDmaBufDescriptor;
struct WrVulkanForeignRgbDescriptor;
struct WrVulkanDmaBufCapabilities;

class VulkanImageCapabilities final {
 public:
  static UniquePtr<VulkanImageCapabilities> Register(WrVulkanExternalImages*);
  static bool Supports(const WrVulkanDmaBufDescriptor&);
  static bool SupportsForeignRGB(const WrVulkanForeignRgbDescriptor&,
                                 uint64_t aDrmMajor, uint64_t aDrmMinor);
  ~VulkanImageCapabilities();

 private:
  explicit VulkanImageCapabilities(WrVulkanDmaBufCapabilities*);
  struct Deleter {
    void operator()(WrVulkanDmaBufCapabilities*) const;
  };
  UniquePtr<WrVulkanDmaBufCapabilities, Deleter> mCapabilities;
};

bool ValidateVulkanImagePublication(const layers::VulkanImagePublication&);
bool ValidateVulkanImageReturn(const layers::VulkanImageReturnMessage&,
                               const layers::VulkanImagePublication&);

// Creation may precede render-thread registration. Use and destruction belong
// to the render thread. Rejection does not invoke the callback; success follows
// the texture host's loan contract.
already_AddRefed<RenderTextureHost> CreateVulkanImageHost(
    const layers::VulkanImagePublication&,
    std::function<void(layers::VulkanImageReturnMessage&&)>&&);

}  // namespace mozilla::wr

#endif
