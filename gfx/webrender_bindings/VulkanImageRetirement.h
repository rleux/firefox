/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_WR_VULKAN_IMAGE_RETIREMENT_H
#define MOZILLA_WR_VULKAN_IMAGE_RETIREMENT_H

#include <deque>
#include <functional>

#include "RenderTextureHost.h"
#include "base/timer.h"

namespace mozilla::wr {
class VulkanImageRetirement final {
 public:
  using Callback = std::function<void(uint64_t, WrVulkanReleaseStatus)>;
  void Add(VulkanImageRelease&&, Callback&&);
  void Defer(std::function<void()>&&);
  void Schedule();
  void Poll();
  void BeginShutdown();
  void FinishShutdown();

 private:
  struct Entry {
    VulkanImageRelease mRelease;
    Callback mNotify;
  };
  std::deque<Entry> mPending;
  std::deque<std::function<void()>> mCallbacks;
  base::RepeatingTimer<VulkanImageRetirement> mTimer;
  bool mShuttingDown = false;
};
}  // namespace mozilla::wr
#endif
