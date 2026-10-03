/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "gtest/gtest.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "mozilla/widget/CompositorWidget.h"

using namespace mozilla;
using namespace mozilla::wr;

namespace {
struct OwnerCounts {
  int mWindow = 0;
  int mDisplay = 0;
  int mRetains = 0;
};

WrVulkanConfig MakeVulkanConfig(OwnerCounts& aCounts) {
  WrVulkanOwner window{&aCounts,
                       [](void* aOwner) {
                         auto& counts = *static_cast<OwnerCounts*>(aOwner);
                         ++counts.mWindow;
                         ++counts.mRetains;
                       },
                       [](void* aOwner) {
                         auto& counts = *static_cast<OwnerCounts*>(aOwner);
                         EXPECT_GT(counts.mDisplay, 0);
                         --counts.mWindow;
                       }};
  WrVulkanOwner display{
      &aCounts,
      [](void* aOwner) {
        auto& counts = *static_cast<OwnerCounts*>(aOwner);
        ++counts.mDisplay;
        ++counts.mRetains;
      },
      [](void* aOwner) { --static_cast<OwnerCounts*>(aOwner)->mDisplay; }};
  return {WrWindowHandle::Win32(nullptr, nullptr),
          window,
          display,
          false,
          true,
          false};
}
}  // namespace

TEST(VulkanSurfaceConfig, RetainsOwnersAndReleasesWindowFirst)
{
  OwnerCounts counts;
  {
    OwnedVulkanConfig config{MakeVulkanConfig(counts)};
    EXPECT_EQ(counts.mWindow, 1);
    EXPECT_EQ(counts.mDisplay, 1);
    EXPECT_EQ(config.Raw().window_owner.object, &counts);
  }
  EXPECT_EQ(counts.mWindow, 0);
  EXPECT_EQ(counts.mDisplay, 0);
}

TEST(VulkanSurfaceConfig, MoveTransfersLeases)
{
  OwnerCounts counts;
  Maybe<OwnedVulkanConfig> original;
  original.emplace(MakeVulkanConfig(counts));
  {
    OwnedVulkanConfig moved(std::move(original.ref()));
    original.reset();
    EXPECT_EQ(counts.mWindow, 1);
    EXPECT_EQ(counts.mDisplay, 1);
    EXPECT_EQ(counts.mRetains, 2);
  }
  EXPECT_EQ(counts.mWindow, 0);
  EXPECT_EQ(counts.mDisplay, 0);
}

TEST(VulkanSurfaceConfig, UnattachedCompositorKeepsInitialConfig)
{
  OwnerCounts counts;
  {
    RenderCompositorVulkan compositor(nullptr, MakeVulkanConfig(counts));
    EXPECT_TRUE(compositor.UsesBackendPresentation());
    EXPECT_FALSE(compositor.SetSurface(nullptr));
    EXPECT_NE(compositor.GetVulkanConfig(), nullptr);
    EXPECT_EQ(counts.mWindow, 1);
    EXPECT_EQ(counts.mDisplay, 1);
  }
  EXPECT_EQ(counts.mWindow, 0);
  EXPECT_EQ(counts.mDisplay, 0);
}

TEST(VulkanSurfaceConfig, DisplayOnlyMoveTransfersLease)
{
  OwnerCounts counts;
  auto raw = MakeVulkanConfig(counts);
  raw.window_owner = {};
  Maybe<OwnedVulkanConfig> original;
  original.emplace(raw);
  {
    OwnedVulkanConfig moved(std::move(original.ref()));
    original.reset();
    EXPECT_FALSE(moved.HasWindow());
    EXPECT_EQ(counts.mWindow, 0);
    EXPECT_EQ(counts.mDisplay, 1);
    EXPECT_EQ(counts.mRetains, 1);
  }
  EXPECT_EQ(counts.mDisplay, 0);
}

TEST(VulkanSurfaceConfig, WindowWithoutDisplayOwner)
{
  OwnerCounts counts;
  auto raw = MakeVulkanConfig(counts);
  raw.display_owner = {};
  raw.window_owner.release = [](void* aOwner) {
    --static_cast<OwnerCounts*>(aOwner)->mWindow;
  };
  {
    OwnedVulkanConfig config(raw);
    EXPECT_TRUE(config.HasWindow());
    EXPECT_EQ(counts.mWindow, 1);
    EXPECT_EQ(counts.mDisplay, 0);
    EXPECT_EQ(counts.mRetains, 1);
  }
  EXPECT_EQ(counts.mWindow, 0);
}

TEST(VulkanSurfaceConfig, DeferredCompositorRemainsPaused)
{
  OwnerCounts counts;
  auto raw = MakeVulkanConfig(counts);
  raw.window_owner = {};
  {
    RenderCompositorVulkan compositor(nullptr, raw);
    EXPECT_NE(compositor.GetVulkanConfig(), nullptr);
    EXPECT_TRUE(compositor.UsesBackendPresentation());
    EXPECT_TRUE(compositor.IsPaused());
    EXPECT_FALSE(compositor.Resume());
    EXPECT_EQ(counts.mWindow, 0);
    EXPECT_EQ(counts.mDisplay, 1);
  }
  EXPECT_EQ(counts.mDisplay, 0);
}

TEST(VulkanSurfaceConfig, DeferredAndroidNeedsNoNativeOwners)
{
  WrVulkanConfig raw{
      WrWindowHandle::Android(nullptr), {}, {}, false, true, false};
  RenderCompositorVulkan compositor(nullptr, raw);
  EXPECT_NE(compositor.GetVulkanConfig(), nullptr);
  EXPECT_TRUE(compositor.UsesBackendPresentation());
  EXPECT_TRUE(compositor.IsPaused());
  EXPECT_FALSE(compositor.Resume());
}
