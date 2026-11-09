#version 450

// the GX2 build of Zakuro's raster.vert: vertices the CPU already shaded,
// clipped and placed on the target, with w kept so the varyings interpolate
// with perspective. they come in Vulkan's convention, y = -1 at the image's
// first row; GX2 puts y = +1 there, so y is turned over. z goes out in
// OpenGL's -w..w, which GX2SetViewport's 0..1 depth range maps back to the
// mapped depth exactly, the value Vulkan's 0..w gives.
layout(location = 0) in vec4 position;
layout(location = 1) in vec4 color;
layout(location = 2) in vec4 texcoords01;
layout(location = 3) in vec4 texcoord2_depth;
layout(location = 4) in vec4 quaternion;
layout(location = 5) in vec4 view;

layout(location = 0) out vec4 out_color;
layout(location = 1) out vec4 out_texcoords01;
layout(location = 2) out vec2 out_texcoord2;
layout(location = 3) noperspective out float out_depth;
layout(location = 4) out vec4 out_quaternion;
layout(location = 5) out vec4 out_view;
// 1/w for raster.frag's w-buffer, in place of gl_FragCoord.w
layout(location = 6) noperspective out float out_inv_w;

void main() {
    gl_Position = vec4(position.x, -position.y, (2.0 * texcoord2_depth.z - 1.0) * position.w, position.w);
    out_inv_w = 1.0 / position.w;
    out_color = color;
    out_texcoords01 = texcoords01;
    out_texcoord2 = texcoord2_depth.xy;
    out_depth = texcoord2_depth.z;
    out_quaternion = quaternion;
    out_view = view;
}
