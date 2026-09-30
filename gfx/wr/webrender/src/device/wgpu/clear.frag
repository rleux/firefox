#version 450
layout(set = 0, binding = 0) uniform ClearParameters {
    vec4 color;
    vec4 depth;
} params;
layout(location = 0) out vec4 oColor;

void main() {
    oColor = params.color;
}
