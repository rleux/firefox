/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef widget_gtk_VulkanProcess_h
#define widget_gtk_VulkanProcess_h

#include <signal.h>
#include <unistd.h>

#include <cerrno>
#ifdef XP_LINUX
#  include <poll.h>
#  include <sys/syscall.h>
#endif
#include "mozilla/ScopeExit.h"

namespace mozilla::widget {
inline bool VulkanProcessHasExited(pid_t aPid) {
  if (aPid <= 0 || aPid == getpid()) {
    return false;
  }
#if defined(XP_LINUX) && defined(SYS_pidfd_open)
  int fd = syscall(SYS_pidfd_open, aPid, 0);
  if (fd >= 0) {
    auto closeFd = MakeScopeExit([&] { close(fd); });
    pollfd state{fd, POLLIN, 0};
    // Poll observes exit (including zombies) without consuming the host's
    // reaper.
    return poll(&state, 1, 0) > 0 && (state.revents & POLLIN);
  }
  if (errno == ESRCH) {
    return true;
  }
#endif
  return kill(aPid, 0) < 0 && errno == ESRCH;
}

}  // namespace mozilla::widget
#endif
