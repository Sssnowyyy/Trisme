#version 450

// one triangle over the whole target, for moving depth in and out. the
// corners come in as (-1, -1), (3, -1), (-1, 3) in Vulkan's convention,
// turned over like raster.vert's, so uv (0, 0) is the first row's first
// pixel on both sides.
layout(location = 0) in vec2 position;

layout(location = 0) out vec2 out_uv;

void main() {
    gl_Position = vec4(position.x, -position.y, 0.0, 1.0);
    out_uv = position * 0.5 + 0.5;
}
