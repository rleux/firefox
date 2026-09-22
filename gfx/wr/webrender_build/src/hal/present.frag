/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#version 450
#extension GL_EXT_samplerless_texture_functions : require
layout(set = 0, binding = 0, std140) uniform Presentation {
    uvec4 source_rect;
    uvec4 target_rect;
    uvec4 encoding;
};
layout(set = 0, binding = 1) uniform texture2D source_texture;
layout(location = 0) out vec4 output_color;
void main() {
    uvec2 pixel = uvec2(gl_FragCoord.xy) - target_rect.xy;
    uvec2 coordinate = source_rect.xy + ((2u * pixel + 1u) * source_rect.zw) / (2u * target_rect.zw);
    vec4 color = texelFetch(source_texture, ivec2(coordinate), 0);
    if (encoding.x != 0u) {
        color.rgb = mix(pow((color.rgb + 0.055) / 1.055, vec3(2.4)), color.rgb / 12.92, lessThanEqual(color.rgb, vec3(0.04045)));
    }
    output_color = color;
}
