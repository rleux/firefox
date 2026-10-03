/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "gtest/gtest.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"

using namespace mozilla::wr;

TEST(VulkanFrameTracker, SubmissionGaps)
{
  VulkanFrameTracker frames;
  ASSERT_TRUE(frames.AddFrame(RenderedFrameId{3}, {5, 0}));
  ASSERT_TRUE(frames.AddFrame(RenderedFrameId{4}, {9, 4}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{1});
  ASSERT_TRUE(frames.Poll({9, 5}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{3});
  ASSERT_TRUE(frames.Poll({9, 8}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{3});
  ASSERT_TRUE(frames.Poll({9, 9}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{4});
  EXPECT_FALSE(frames.HasPendingFrames());
}

TEST(VulkanFrameTracker, SharedAndEmptySubmissions)
{
  VulkanFrameTracker frames;
  ASSERT_TRUE(frames.AddFrame(RenderedFrameId{3}, {0, 0}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{3});
  ASSERT_TRUE(frames.AddFrame(RenderedFrameId{4}, {7, 0}));
  for (uint64_t id = 5; id <= 100; ++id) {
    ASSERT_TRUE(frames.AddFrame(RenderedFrameId{id}, {7, 0}));
  }
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{3});
  ASSERT_TRUE(frames.Poll({7, 7}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{100});
  ASSERT_TRUE(frames.AddFrame(RenderedFrameId{101}, {7, 7}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{101});
  EXPECT_FALSE(frames.HasPendingFrames());
}

TEST(VulkanFrameTracker, InvalidProgressDoesNotRetireFrames)
{
  VulkanFrameTracker frames;
  ASSERT_TRUE(frames.AddFrame(RenderedFrameId{3}, {8, 4}));
  EXPECT_FALSE(frames.Poll({8, 9}));
  EXPECT_FALSE(frames.Poll({7, 4}));
  EXPECT_FALSE(frames.Poll({8, 3}));
  EXPECT_FALSE(frames.AddFrame(RenderedFrameId{3}, {8, 8}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{1});
  EXPECT_TRUE(frames.HasPendingFrames());
  ASSERT_TRUE(frames.Poll({8, 8}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{3});
}

TEST(VulkanFrameTracker, InitializationWorkHasNoFrame)
{
  VulkanFrameTracker frames;
  ASSERT_TRUE(frames.Poll({2, 0}));
  EXPECT_FALSE(frames.HasPendingFrames());
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{1});
  ASSERT_TRUE(frames.AddFrame(RenderedFrameId{3}, {2, 0}));
  ASSERT_TRUE(frames.Poll({2, 2}));
  EXPECT_EQ(frames.CompletedFrame(), RenderedFrameId{3});
}
