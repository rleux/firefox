/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#version 450
layout(location = 0) in uint instance_color;
layout(location = 0) flat out vec4 color;
void main() {
    vec2 positions[4] = vec2[](vec2(-1, -1), vec2(1, -1), vec2(-1, 1), vec2(1, 1));
    gl_Position = vec4(positions[gl_VertexIndex], 0, 1);
    color = vec4(instance_color & 255u, (instance_color >> 8u) & 255u,
                 (instance_color >> 16u) & 255u, instance_color >> 24u) / 255.0;
}
