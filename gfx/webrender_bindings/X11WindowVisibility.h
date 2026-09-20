/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef MOZILLA_WR_X11WINDOWVISIBILITY_H
#define MOZILLA_WR_X11WINDOWVISIBILITY_H

#include <cstdint>

namespace mozilla::wr {

class X11WindowVisibility final {
 public:
  enum class State { Unknown, Visible, Hidden };

  X11WindowVisibility(void* aDisplay, uintptr_t aWindow)
      : mDisplay(aDisplay), mWindow(aWindow) {}
  State Query();

 private:
  void* mDisplay;
  uintptr_t mWindow;
  uint32_t mStateAtom = 0;
  uint32_t mHiddenAtom = 0;
};

}  // namespace mozilla::wr

#endif
