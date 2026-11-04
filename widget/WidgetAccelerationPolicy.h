/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#ifndef widget_WidgetAccelerationPolicy_h
#define widget_WidgetAccelerationPolicy_h

namespace mozilla::widget {

constexpr bool CanAccelerateWidget(bool aSupportsAcceleration,
                                   bool aRequiresSoftware,
                                   bool aForceAcceleration) {
  return !aRequiresSoftware && (aSupportsAcceleration || aForceAcceleration);
}

}  // namespace mozilla::widget

#endif
