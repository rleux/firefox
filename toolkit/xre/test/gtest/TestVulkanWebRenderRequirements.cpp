/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "VulkanWindowPolicy.h"
#include "VulkanProcess.h"
#ifdef XP_LINUX
#  include <sys/wait.h>
#endif
#include "WidgetAccelerationPolicy.h"
#include "gtest/gtest.h"

TEST(VulkanWebRenderRequirements, WindowPolicyPreservesWorkingBackends)
{
  using namespace mozilla::widget;
  for (bool topLevel : {false, true}) {
    for (bool explicitAlpha : {false, true}) {
      EXPECT_EQ(SelectVulkanWindowPolicy(false, false, topLevel, explicitAlpha),
                VulkanWindowPolicy::ExistingVisual);
      EXPECT_EQ(SelectVulkanWindowPolicy(true, true, topLevel, explicitAlpha),
                VulkanWindowPolicy::ExistingVisual);
    }
  }
}

TEST(VulkanWebRenderRequirements, OpaqueFallbackKeepsTransparentWindowsSoftware)
{
  using namespace mozilla::widget;
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, true, false),
            VulkanWindowPolicy::OpaqueVisual);
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, true, true),
            VulkanWindowPolicy::Software);
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, false, false),
            VulkanWindowPolicy::Software);
  EXPECT_EQ(SelectVulkanWindowPolicy(true, false, false, true),
            VulkanWindowPolicy::Software);
}

TEST(VulkanWebRenderRequirements, SoftwareRequirementCannotBeForced)
{
  using namespace mozilla::widget;
  for (bool supports : {false, true}) {
    for (bool force : {false, true}) {
      EXPECT_FALSE(CanAccelerateWidget(supports, true, force));
      EXPECT_EQ(CanAccelerateWidget(supports, false, force), supports || force);
    }
  }
  const auto policy = SelectVulkanWindowPolicy(true, false, false, true);
  EXPECT_FALSE(
      CanAccelerateWidget(true, policy == VulkanWindowPolicy::Software, true));
}

#ifdef XP_LINUX
TEST(VulkanWebRenderRequirements, ExitObservationDoesNotConsumeProcessReaping)
{
  using namespace mozilla::widget;
  EXPECT_FALSE(VulkanProcessHasExited(0));
  EXPECT_FALSE(VulkanProcessHasExited(getpid()));
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    for (;;) {
      pause();
    }
  }
  bool reaped = false;
  auto cleanup = mozilla::MakeScopeExit([&] {
    if (!reaped) {
      kill(child, SIGKILL);
      int status;
      waitpid(child, &status, 0);
    }
  });
  EXPECT_FALSE(VulkanProcessHasExited(child));
  ASSERT_EQ(kill(child, SIGKILL), 0);
  siginfo_t exited{};
  ASSERT_EQ(waitid(P_PID, child, &exited, WEXITED | WNOWAIT), 0);
#  ifdef SYS_pidfd_open
  int fd = syscall(SYS_pidfd_open, child, 0);
  if (fd >= 0) {
    close(fd);
    EXPECT_TRUE(VulkanProcessHasExited(child));
  }
#  endif
  int status = 0;
  ASSERT_EQ(waitpid(child, &status, 0), child);
  reaped = true;
  EXPECT_TRUE(WIFSIGNALED(status));
  EXPECT_TRUE(VulkanProcessHasExited(child));
}
#endif
