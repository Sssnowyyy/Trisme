#version 450

// the reverse of depth_load.frag: a depth buffer, sampled as a texture,
// into an RGBA8 color buffer, the 24-bit depth's low byte in r, a = 0.
layout(location = 0) in vec2 in_uv;

layout(binding = 0) uniform sampler2D depth;

layout(location = 0) out vec4 out_color;

void main() {
    float value = floor(clamp(texture(depth, in_uv).r, 0.0, 1.0) * 16777215.0 + 0.5);
    float high = floor(value / 65536.0);
    float middle = floor((value - high * 65536.0) / 256.0);
    float low = value - high * 65536.0 - middle * 256.0;
    out_color = vec4(low, middle, high, 0.0) / 255.0;
}
