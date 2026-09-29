/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#version 450
layout(location = 0) flat in vec4 color;
layout(location = 0) out vec4 result;
void main() {
    result = color;
}
