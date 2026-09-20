/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "X11WindowVisibility.h"

#include <X11/Xlib-xcb.h>
#include <xcb/xcb.h>

#include "mozilla/UniquePtrExtensions.h"

namespace mozilla::wr {

X11WindowVisibility::State X11WindowVisibility::Query() {
  if (!mDisplay || !mWindow || mWindow > UINT32_MAX) {
    return State::Unknown;
  }
  auto* display = static_cast<Display*>(mDisplay);
  auto* connection = XGetXCBConnection(display);
  if (!connection || xcb_connection_has_error(connection)) {
    return State::Unknown;
  }
  XFlush(display);
  if (xcb_connection_has_error(connection)) {
    return State::Unknown;
  }
  if (!mStateAtom || !mHiddenAtom) {
    constexpr char stateName[] = "_NET_WM_STATE";
    constexpr char hiddenName[] = "_NET_WM_STATE_HIDDEN";
    auto stateCookie =
        xcb_intern_atom(connection, false, sizeof(stateName) - 1, stateName);
    auto hiddenCookie =
        xcb_intern_atom(connection, false, sizeof(hiddenName) - 1, hiddenName);
    xcb_generic_error_t* stateError = nullptr;
    xcb_generic_error_t* hiddenError = nullptr;
    UniqueFreePtr<xcb_intern_atom_reply_t> stateReply(
        xcb_intern_atom_reply(connection, stateCookie, &stateError));
    UniqueFreePtr<xcb_generic_error_t> freeStateError(stateError);
    UniqueFreePtr<xcb_intern_atom_reply_t> hiddenReply(
        xcb_intern_atom_reply(connection, hiddenCookie, &hiddenError));
    UniqueFreePtr<xcb_generic_error_t> freeHiddenError(hiddenError);
    if (!stateReply || !hiddenReply || stateError || hiddenError) {
      return State::Unknown;
    }
    mStateAtom = stateReply->atom;
    mHiddenAtom = hiddenReply->atom;
    if (!mStateAtom || !mHiddenAtom) {
      return State::Unknown;
    }
  }

  const auto window = static_cast<xcb_window_t>(mWindow);
  auto attributesCookie = xcb_get_window_attributes(connection, window);
  auto propertyCookie = xcb_get_property(connection, false, window, mStateAtom,
                                         XCB_ATOM_ATOM, 0, 64);
  xcb_generic_error_t* attributesError = nullptr;
  xcb_generic_error_t* propertyError = nullptr;
  UniqueFreePtr<xcb_get_window_attributes_reply_t> attributes(
      xcb_get_window_attributes_reply(connection, attributesCookie,
                                      &attributesError));
  UniqueFreePtr<xcb_generic_error_t> freeAttributesError(attributesError);
  UniqueFreePtr<xcb_get_property_reply_t> property(
      xcb_get_property_reply(connection, propertyCookie, &propertyError));
  UniqueFreePtr<xcb_generic_error_t> freePropertyError(propertyError);
  if (!attributes || !property || attributesError || propertyError ||
      xcb_connection_has_error(connection)) {
    return State::Unknown;
  }
  if (attributes->map_state == XCB_MAP_STATE_UNMAPPED ||
      attributes->map_state == XCB_MAP_STATE_UNVIEWABLE) {
    return State::Hidden;
  }
  if (attributes->map_state != XCB_MAP_STATE_VIEWABLE) {
    return State::Unknown;
  }
  if (property->type == XCB_ATOM_NONE) {
    return State::Visible;
  }
  if (property->type != XCB_ATOM_ATOM || property->format != 32 ||
      property->value_len > 64 ||
      xcb_get_property_value_length(property.get()) !=
          static_cast<int>(property->value_len * sizeof(xcb_atom_t))) {
    return State::Unknown;
  }
  auto* states =
      static_cast<const xcb_atom_t*>(xcb_get_property_value(property.get()));
  for (uint32_t i = 0; i < property->value_len; ++i) {
    if (states[i] == mHiddenAtom) {
      return State::Hidden;
    }
  }
  return property->bytes_after ? State::Unknown : State::Visible;
}

}  // namespace mozilla::wr
