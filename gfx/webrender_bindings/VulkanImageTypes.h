/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_GFX_VULKANIMAGETYPES_H
#define MOZILLA_GFX_VULKANIMAGETYPES_H

#include <array>
#include <cstdint>

#include "mozilla/UniquePtrExtensions.h"

namespace mozilla::wr {

using VulkanUUID = std::array<uint8_t, 16>;

enum class VulkanImageReturnStatus { Unused, Submitted, Abandoned };

struct VulkanImageReturn {
  VulkanImageReturnStatus mStatus = VulkanImageReturnStatus::Unused;
  UniqueFileHandle mSemaphore;
  VulkanUUID mDeviceUUID{};
  VulkanUUID mDriverUUID{};
  uint64_t mValue = 0;
};

}  // namespace mozilla::wr

#endif
