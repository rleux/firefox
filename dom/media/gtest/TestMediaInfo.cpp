/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this file,
 * You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "MediaInfo.h"
#include "chrome/common/ipc_message.h"
#include "chrome/common/ipc_message_utils.h"
#include "gtest/gtest.h"
#include "mozilla/dom/MediaIPCUtils.h"

using namespace mozilla;

TEST(TestMediaInfo, CloneVideoInfo)
{
  VideoInfo info;
  info.mDisplay = info.mImage = gfx::IntSize{640, 360};
  info.mStereoMode = StereoMode::BOTTOM_TOP;
  info.mRotation = VideoRotation::kDegree_270;
  info.mColorDepth = gfx::ColorDepth::COLOR_16;
  info.mColorRange = gfx::ColorRange::FULL;
  info.mChromaLocation = VideoInfo::ChromaLocation::Center;

  UniquePtr<TrackInfo> clone1 = info.Clone();
  UniquePtr<TrackInfo> clone2 = info.Clone();

  auto* info1 = clone1->GetAsVideoInfo();
  auto* info2 = clone2->GetAsVideoInfo();

  EXPECT_EQ(info1->mDisplay, info2->mDisplay);
  EXPECT_EQ(info1->mImage, info2->mImage);
  EXPECT_EQ(info1->mStereoMode, info2->mStereoMode);
  EXPECT_EQ(info1->mRotation, info2->mRotation);
  EXPECT_EQ(info1->mColorDepth, info2->mColorDepth);
  EXPECT_EQ(info1->mColorRange, info2->mColorRange);
  EXPECT_EQ(info1->mChromaLocation, info2->mChromaLocation);
  // They should have their own media byte buffers which have different address
  EXPECT_NE(info1->mExtraData.get(), info2->mExtraData.get());
  EXPECT_NE(info1->mCodecSpecificConfig.get(),
            info2->mCodecSpecificConfig.get());
}

TEST(TestMediaInfo, ChromaLocationCopyEqualityAndIPC)
{
  using ChromaLocation = VideoInfo::ChromaLocation;
  constexpr ChromaLocation locations[] = {
      ChromaLocation::Unspecified, ChromaLocation::Left,
      ChromaLocation::Center,      ChromaLocation::TopLeft,
      ChromaLocation::Top,         ChromaLocation::Unsupported,
  };
  for (const auto location : locations) {
    SCOPED_TRACE(static_cast<int>(location));
    VideoInfo input(640, 360);
    input.mChromaLocation = location;
    VideoInfo copied(input);
    EXPECT_EQ(copied, input);
    ASSERT_EQ(copied.mChromaLocation, location);
    auto cloned = input.Clone();
    ASSERT_TRUE(cloned->GetAsVideoInfo());
    EXPECT_EQ(*cloned->GetAsVideoInfo(), input);
    EXPECT_EQ(cloned->GetAsVideoInfo()->mChromaLocation, location);

    IPC::Message message(MSG_ROUTING_NONE, 0);
    {
      IPC::MessageWriter writer(message);
      IPC::WriteParam(&writer, input);
    }
    IPC::MessageReader reader(message);
    VideoInfo output;
    ASSERT_TRUE(IPC::ReadParam(&reader, &output));
    EXPECT_EQ(output, input);
    EXPECT_EQ(output.mChromaLocation, location);

    VideoInfo different(input);
    different.mChromaLocation = location == ChromaLocation::Center
                                    ? ChromaLocation::Left
                                    : ChromaLocation::Center;
    EXPECT_NE(different, input);
  }
}

TEST(TestMediaInfo, ChromaLocationIPCRejectsInvalidValue)
{
  IPC::Message message(MSG_ROUTING_NONE, 0);
  {
    IPC::MessageWriter writer(message);
    IPC::WriteParam(&writer, uint8_t{255});
  }
  IPC::MessageReader reader(message);
  VideoInfo::ChromaLocation output;
  EXPECT_FALSE(IPC::ReadParam(&reader, &output));
}
