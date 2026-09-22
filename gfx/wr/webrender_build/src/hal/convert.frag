#version 450
layout(set = 0, binding = 0) uniform sampler2D source_image;
layout(push_constant) uniform Params { uint alpha_mode; } params;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 color;
void main() {
    color = texture(source_image, uv);
    if (params.alpha_mode == 0u) color.a = 1.0;
    if (params.alpha_mode == 2u) color.rgb *= color.a;
}
