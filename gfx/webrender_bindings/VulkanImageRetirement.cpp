/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanImageRetirement.h"

#include "RenderThread.h"

namespace mozilla::wr {
void VulkanImageRetirement::Add(VulkanImageRelease&& aRelease,
                                Callback&& aNotify) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  mShuttingDown |= RenderThread::Get()->HasShutdown();
  mPending.push_back({std::move(aRelease), std::move(aNotify)});
  Schedule();
}
void VulkanImageRetirement::Defer(std::function<void()>&& aCallback) {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  mShuttingDown |= RenderThread::Get()->HasShutdown();
  mCallbacks.push_back(std::move(aCallback));
  Schedule();
}
void VulkanImageRetirement::Schedule() {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  if (!mShuttingDown && !mTimer.IsRunning() &&
      !RenderThread::Get()->HasShutdown()) {
    mTimer.Start(base::TimeDelta::FromMilliseconds(2), this,
                 &VulkanImageRetirement::Poll);
  }
}
void VulkanImageRetirement::Poll() {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  const bool nativePending = wr_vulkan_process_native_work(nullptr);
  size_t remaining = mPending.size();
  while (remaining--) {
    auto entry = std::move(mPending.front());
    mPending.pop_front();
    auto status = entry.mRelease.GetStatus();
    if (status == WrVulkanReleaseStatus::Pending) {
      mPending.push_back(std::move(entry));
    } else {
      entry.mNotify(entry.mRelease.GetValue(), status);
    }
  }
  remaining = mCallbacks.size();
  while (remaining--) {
    auto callback = std::move(mCallbacks.front());
    mCallbacks.pop_front();
    callback();
  }
  if (!nativePending && mPending.empty() && mCallbacks.empty()) {
    mTimer.Stop();
  }
}
void VulkanImageRetirement::BeginShutdown() {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  mShuttingDown = true;
  mTimer.Stop();
}
void VulkanImageRetirement::FinishShutdown() {
  MOZ_ASSERT(RenderThread::IsInRenderThread());
  BeginShutdown();
  wr_vulkan_process_native_work(nullptr);
  wr_vulkan_shutdown_native_work();
  while (!mPending.empty()) {
    auto entry = std::move(mPending.front());
    mPending.pop_front();
    auto status = entry.mRelease.GetStatus();
    if (status == WrVulkanReleaseStatus::Pending) {
      status = WrVulkanReleaseStatus::Abandoned;
    }
    entry.mNotify(entry.mRelease.GetValue(), status);
  }
  while (!mCallbacks.empty()) {
    auto callback = std::move(mCallbacks.front());
    mCallbacks.pop_front();
    callback();
  }
}
}  // namespace mozilla::wr
