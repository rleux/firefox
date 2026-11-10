/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <sys/wait.h>

#include "../../gtk/VulkanProcess.h"
#include "gtest/gtest.h"

using mozilla::widget::VulkanProcessHasExited;

TEST(VulkanProcess, WaitsForExitWithoutReapingOrAcceptingALiveProcess)
{
  EXPECT_FALSE(VulkanProcessHasExited(0));
  EXPECT_FALSE(VulkanProcessHasExited(getpid()));
  int pipeFds[2];
  ASSERT_EQ(pipe(pipeFds), 0);
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    close(pipeFds[1]);
    char byte;
    _exit(read(pipeFds[0], &byte, 1) == 1 ? 0 : 1);
  }
  close(pipeFds[0]);
  auto cleanup = mozilla::MakeScopeExit([&] {
    close(pipeFds[1]);
    kill(child, SIGKILL);
    waitpid(child, nullptr, 0);
  });
  EXPECT_FALSE(VulkanProcessHasExited(child, 10));
  ASSERT_EQ(write(pipeFds[1], "x", 1), 1);
  EXPECT_TRUE(VulkanProcessHasExited(child, 1000));
  int status = 0;
  ASSERT_EQ(waitpid(child, &status, 0), child);
  EXPECT_TRUE(WIFEXITED(status));
  EXPECT_EQ(WEXITSTATUS(status), 0);
  cleanup.release();
  close(pipeFds[1]);
}
