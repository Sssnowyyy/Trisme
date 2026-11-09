#version 450

// writes a depth buffer from an RGBA8 texture of its size holding each
// pixel's 24-bit depth, the low byte in r. sampled at pixel centers with
// point filtering, so each pixel reads its own texel.
layout(location = 0) in vec2 in_uv;

layout(binding = 0) uniform sampler2D depth_texels;

void main() {
    vec3 bytes = floor(texture(depth_texels, in_uv).rgb * 255.0 + 0.5);
    gl_FragDepth = (bytes.r + bytes.g * 256.0 + bytes.b * 65536.0) / 16777215.0;
}
