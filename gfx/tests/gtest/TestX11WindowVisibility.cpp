/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// clang-format off
#include "gtest/gtest.h"

#include <X11/Xatom.h>
#include <X11/Xlib.h>
// clang-format on

#include <array>
#include <cstdint>
#include <limits>

#include "mozilla/webrender/X11WindowVisibility.h"

namespace mozilla::wr {

class X11WindowVisibilityTest : public testing::Test {
 protected:
  void SetUp() override {
    mDisplay = XOpenDisplay(nullptr);
    if (!mDisplay) {
      GTEST_SKIP() << "Requires an X11 display";
    }
    mRoot = DefaultRootWindow(mDisplay);
    mParent = XCreateSimpleWindow(mDisplay, mRoot, 0, 0, 160, 120, 0, 0, 0);
    mWindow = XCreateSimpleWindow(mDisplay, mParent, 0, 0, 80, 60, 0, 0, 0);
    mOther = XCreateSimpleWindow(mDisplay, mRoot, 200, 0, 80, 60, 0, 0, 0);
    XSetWindowAttributes attributes{};
    attributes.override_redirect = True;
    XChangeWindowAttributes(mDisplay, mParent, CWOverrideRedirect, &attributes);
    XChangeWindowAttributes(mDisplay, mOther, CWOverrideRedirect, &attributes);
    mState = XInternAtom(mDisplay, "_NET_WM_STATE", False);
    mHidden = XInternAtom(mDisplay, "_NET_WM_STATE_HIDDEN", False);
    mOtherState = XInternAtom(mDisplay, "_NET_WM_STATE_MAXIMIZED_VERT", False);
    ASSERT_NE(mState, Atom(None));
    ASSERT_NE(mHidden, Atom(None));
    ASSERT_NE(mOtherState, Atom(None));
  }

  void TearDown() override {
    if (!mDisplay) {
      return;
    }
    if (mOther) {
      XDestroyWindow(mDisplay, mOther);
    }
    if (mWindow) {
      XDestroyWindow(mDisplay, mWindow);
    }
    if (mParent) {
      XDestroyWindow(mDisplay, mParent);
    }
    XSync(mDisplay, False);
    XCloseDisplay(mDisplay);
  }

  void MapAll() {
    XMapWindow(mDisplay, mParent);
    XMapWindow(mDisplay, mWindow);
    XMapWindow(mDisplay, mOther);
    XSync(mDisplay, False);
  }

  void SetStates(const Atom* aStates, int aLength) {
    XChangeProperty(mDisplay, mWindow, mState, XA_ATOM, 32, PropModeReplace,
                    reinterpret_cast<const unsigned char*>(aStates), aLength);
    XSync(mDisplay, False);
  }

  X11WindowVisibility Visibility() const {
    return X11WindowVisibility(mDisplay, mWindow);
  }

  Display* mDisplay = nullptr;
  Window mRoot = None;
  Window mParent = None;
  Window mWindow = None;
  Window mOther = None;
  Atom mState = None;
  Atom mHidden = None;
  Atom mOtherState = None;
};

TEST(X11WindowVisibility, NullInputsAreUnknown)
{
  using State = X11WindowVisibility::State;
  EXPECT_EQ(X11WindowVisibility(nullptr, 1).Query(), State::Unknown);
  EXPECT_EQ(X11WindowVisibility(reinterpret_cast<void*>(1), 0).Query(),
            State::Unknown);
#if UINTPTR_MAX > UINT32_MAX
  EXPECT_EQ(
      X11WindowVisibility(
          reinterpret_cast<void*>(1),
          static_cast<uintptr_t>(std::numeric_limits<uint32_t>::max()) + 1)
          .Query(),
      State::Unknown);
#endif
}

TEST_F(X11WindowVisibilityTest, MapStateAndFocus) {
  using State = X11WindowVisibility::State;
  auto visibility = Visibility();
  EXPECT_EQ(visibility.Query(), State::Hidden);
  MapAll();
  EXPECT_EQ(visibility.Query(), State::Visible);

  XSetInputFocus(mDisplay, mOther, RevertToPointerRoot, CurrentTime);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Visible);
  XSetInputFocus(mDisplay, mWindow, RevertToPointerRoot, CurrentTime);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Visible);

  XUnmapWindow(mDisplay, mWindow);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Hidden);
  XMapWindow(mDisplay, mWindow);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Visible);
}

TEST_F(X11WindowVisibilityTest, UnmappedAncestorIsHidden) {
  using State = X11WindowVisibility::State;
  MapAll();
  auto visibility = Visibility();
  EXPECT_EQ(visibility.Query(), State::Visible);
  XUnmapWindow(mDisplay, mParent);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Hidden);
  XMapWindow(mDisplay, mParent);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Visible);
}

TEST_F(X11WindowVisibilityTest, HiddenClearAndMissingProperties) {
  using State = X11WindowVisibility::State;
  MapAll();
  auto visibility = Visibility();
  EXPECT_EQ(visibility.Query(), State::Visible);

  SetStates(&mHidden, 1);
  EXPECT_EQ(visibility.Query(), State::Hidden);
  SetStates(&mOtherState, 1);
  EXPECT_EQ(visibility.Query(), State::Visible);
  SetStates(nullptr, 0);
  EXPECT_EQ(visibility.Query(), State::Visible);
  XDeleteProperty(mDisplay, mWindow, mState);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Visible);
}

TEST_F(X11WindowVisibilityTest, MalformedPropertyIsUnknown) {
  using State = X11WindowVisibility::State;
  MapAll();
  auto visibility = Visibility();
  constexpr char kText[] = "hidden";
  XChangeProperty(mDisplay, mWindow, mState, XA_STRING, 8, PropModeReplace,
                  reinterpret_cast<const unsigned char*>(kText),
                  sizeof(kText) - 1);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Unknown);

  const uint16_t value = 1;
  XChangeProperty(mDisplay, mWindow, mState, XA_ATOM, 16, PropModeReplace,
                  reinterpret_cast<const unsigned char*>(&value), 1);
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Unknown);
}

TEST_F(X11WindowVisibilityTest, TruncatedPropertyWithoutHiddenIsUnknown) {
  using State = X11WindowVisibility::State;
  MapAll();
  auto visibility = Visibility();
  std::array<Atom, 65> states;
  states.fill(mOtherState);
  states[0] = mHidden;
  SetStates(states.data(), static_cast<int>(states.size()));
  EXPECT_EQ(visibility.Query(), State::Hidden);
  states.fill(mOtherState);
  SetStates(states.data(), static_cast<int>(states.size()));
  EXPECT_EQ(visibility.Query(), State::Unknown);
}

TEST_F(X11WindowVisibilityTest, QueryPreservesXlibEvents) {
  using State = X11WindowVisibility::State;
  MapAll();
  auto visibility = Visibility();
  XEvent sent{};
  sent.xclient.type = ClientMessage;
  sent.xclient.display = mDisplay;
  sent.xclient.window = mWindow;
  sent.xclient.format = 32;
  sent.xclient.data.l[0] = 0x13579;
  ASSERT_NE(XSendEvent(mDisplay, mWindow, False, NoEventMask, &sent), 0);
  XFlush(mDisplay);
  EXPECT_EQ(visibility.Query(), State::Visible);

  XEvent received{};
  ASSERT_NE(XCheckTypedWindowEvent(mDisplay, mWindow, ClientMessage, &received),
            0);
  EXPECT_EQ(received.xclient.data.l[0], 0x13579);
}

TEST_F(X11WindowVisibilityTest, DestroyedWindowIsUnknown) {
  using State = X11WindowVisibility::State;
  MapAll();
  auto visibility = Visibility();
  EXPECT_EQ(visibility.Query(), State::Visible);
  XDestroyWindow(mDisplay, mWindow);
  mWindow = None;
  XSync(mDisplay, False);
  EXPECT_EQ(visibility.Query(), State::Unknown);
}

}  // namespace mozilla::wr
