#version 450
layout(set = 0, binding = 0) uniform ClearParameters {
    vec4 color;
    vec4 depth;
} params;

void main() {
    vec2 corner = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    gl_Position = vec4(corner * 2.0 - 1.0, params.depth.x, 1.0);
}
