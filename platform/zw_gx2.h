/* the GX2 rasterizer's interface, between Zakuro's GPU (Rust,
   zakuro-gpu/src/raster/gx2.rs) and the platform layer (C, platform/gx2.c).

   GX2 belongs to the core that called GX2Init, the main thread's, where
   SDL draws too. so the console's thread never calls GX2 itself: it fills
   surfaces and textures it owns in memory both sides see, records commands
   in plain structs and submits them. the main thread runs the batch between
   SDL's frames, waits for the GPU and hands the batch back. a submit blocks
   until then, so outside zw_gx2_submit the GPU never touches what the
   console's thread holds, and that thread may read and write the pixels of
   its surfaces freely.

   conventions:
   - surfaces are RGBA8, linear, `pitch` pixels from one row to the next,
     rows in the order the Vulkan backend keeps its images: row 0 is NDC
     y = -1, which is the PICA window's bottom row and the last row of the
     buffer in guest memory. the platform layer never turns them over; it
     maps NDC y = -1 to row 0. a texture's levels each get their own pointer
     and pitch, row 0 being t = 0.
   - vertices are ZW_GX2_VERTEX_SIZE bytes, six vec4s of native-endian
     floats: clip-space position (Vulkan convention: y = -1 is row 0, z in
     0..w), color, texture coordinates 0 and 1, coordinate 2 with the mapped
     depth in z, the quaternion, the view vector. the layout raster.vert
     reads.
   - indices are native-endian uint32, three a triangle.
   - uniform blocks are the std140 blocks raster.frag declares, Draw
     (binding 0) and Tables (binding 1), each 32-bit word already
     byte-swapped to little-endian, which is how the GPU reads them.
   - fixed-function values are the PICA200's own encodings; the platform
     layer maps them to GX2's.
   - everything a command points at (vertices, indices, uniforms) comes from
     zw_gx2_alloc and stays valid until the submit returns. */

#ifndef ZW_GX2_H
#define ZW_GX2_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define ZW_GX2_VERTEX_SIZE 96u
/* the Tables block, 7680 vec2 packed two to a vec4: 24 lighting tables of
   256 entries, the procedural texture's noise, color and alpha maps (128
   each), its 256 colors and their 256 steps (two vec2 an entry each), the
   fog's 128 entries */
#define ZW_GX2_TABLES_SIZE 61440u
#define ZW_GX2_MAX_LEVELS 8u

typedef struct ZwGx2Level {
    uint8_t *pixels;
    uint32_t pitch; /* pixels from one row to the next */
    uint32_t width, height;
} ZwGx2Level;

/* pixel shader variants, compiled ahead of time from raster.frag */
enum {
    ZW_GX2_SHADER_LIGHTING = 1u << 0,
    ZW_GX2_SHADER_PROCTEX = 1u << 1,
    ZW_GX2_SHADER_WRITES_DEPTH = 1u << 2,
};

enum {
    /* draws triangles into color (and depth, when not 0) */
    ZW_GX2_CMD_DRAW = 1,
    /* fills color with clear_color */
    ZW_GX2_CMD_CLEAR_COLOR = 2,
    /* fills depth with clear_depth and, when clear_flags & 2, the stencil
       with clear_stencil; clear_flags & 1 for the depth */
    ZW_GX2_CMD_CLEAR_DEPTH = 3,
    /* writes depth from textures[0], an RGBA8 texture of the target's size
       holding each pixel's 24-bit depth in r (low byte), g, b and the
       stencil in a. the stencil is left alone: the GPU can't write it from
       a shader, clear it first when it matters */
    ZW_GX2_CMD_LOAD_DEPTH = 4,
    /* the reverse, depth into color, packed the same way, a = 0 */
    ZW_GX2_CMD_STORE_DEPTH = 5,
};

typedef struct ZwGx2Sampler {
    uint8_t mag_linear;
    uint8_t min_linear;
    /* 0 one level only, 1 nearest level, 2 between levels */
    uint8_t mip;
    /* 0 clamp to edge, 1 clamp to border (as edge: raster.frag applies the
       border color itself), 2 repeat, 3 mirrored repeat */
    uint8_t wrap_s, wrap_t;
    uint8_t pad_[3];
    float min_lod, max_lod, lod_bias;
} ZwGx2Sampler;

typedef struct ZwGx2Command {
    uint32_t kind;
    /* the targets, handles from zw_gx2_color_create and zw_gx2_depth_create,
       0 for none. a draw's two have the same size */
    uint32_t color, depth;
    /* what may be drawn: x0, y0, x1, y1 in target pixels, rows counted
       from row 0 as above, x1 and y1 excluded. draws cover the whole
       target: the viewport is always all of it, depth 0..1 */
    int32_t scissor[4];

    /* ZW_GX2_CMD_DRAW */
    const void *vertices;
    uint32_t vertex_count;
    const uint32_t *indices; /* NULL: the vertices in order, three a triangle */
    uint32_t index_count;
    const void *uniforms; /* Draw, little-endian words */
    uint32_t uniforms_size;
    const void *tables; /* Tables, ZW_GX2_TABLES_SIZE, or NULL when unused */
    uint32_t textures[3]; /* handles, 0 for none */
    ZwGx2Sampler samplers[3];
    uint32_t shader; /* ZW_GX2_SHADER_* */

    uint8_t color_mask; /* 1 red, 2 green, 4 blue, 8 alpha */
    uint8_t depth_test;
    uint8_t depth_func; /* PICA: 0 never 1 always 2 eq 3 ne 4 lt 5 le 6 gt 7 ge */
    uint8_t depth_write;
    uint8_t stencil_test;
    uint8_t stencil_func; /* as depth_func */
    uint8_t stencil_ref;
    uint8_t stencil_mask; /* compared bits */
    uint8_t stencil_write_mask;
    /* PICA: 0 keep 1 zero 2 replace 3 incr 4 decr 5 invert 6 incr wrap 7 decr wrap */
    uint8_t stencil_fail, stencil_zfail, stencil_zpass;
    uint8_t blend_enable;
    uint8_t blend_eq_rgb, blend_eq_alpha; /* PICA: 0 add 1 sub 2 rev sub 3 min 4 max */
    /* PICA: 0 zero 1 one 2 src 3 1-src 4 dst 5 1-dst 6 src a 7 1-src a 8 dst a
       9 1-dst a 10 const 11 1-const 12 const a 13 1-const a 14 src a saturate */
    uint8_t blend_src_rgb, blend_dst_rgb, blend_src_alpha, blend_dst_alpha;
    uint8_t logic_op_enable;
    uint8_t logic_op; /* PICA: 0 clear 1 and 2 and rev 3 copy 4 set 5 copy inv 6 noop
                         7 invert 8 nand 9 or 10 nor 11 xor 12 equiv 13 and inv 14 or rev 15 or inv */
    uint8_t pad_[2];
    uint32_t blend_color; /* r | g << 8 | b << 16 | a << 24 */

    /* ZW_GX2_CMD_CLEAR_* */
    float clear_color[4];
    float clear_depth;
    uint8_t clear_stencil;
    uint8_t clear_flags;
    uint8_t pad2_[2];
} ZwGx2Command;

/* console's thread ---------------------------------------------------- */

/* 1 when the GX2 rasterizer can be used: the shaders are in the program and
   loaded, and the main thread services batches */
int32_t zw_gx2_available(void);

/* an RGBA8 render target the CPU can read and write, its single level in
   *level. 0 when out of memory */
uint32_t zw_gx2_color_create(uint32_t width, uint32_t height, ZwGx2Level *level);
/* a 24-bit depth, 8-bit stencil target. 0 when out of memory */
uint32_t zw_gx2_depth_create(uint32_t width, uint32_t height);
/* an RGBA8 texture with `levels` sizes, each half the last, its levels in
   level_out[0..levels). 0 when out of memory */
uint32_t zw_gx2_texture_create(uint32_t width, uint32_t height, uint32_t levels, ZwGx2Level *level_out);
/* the CPU wrote a surface's or texture's pixels; the next submit makes the
   GPU see them */
void zw_gx2_written(uint32_t handle);
void zw_gx2_destroy(uint32_t handle);

/* room for what the next batch points at, until zw_gx2_submit returns,
   aligned to at least 256 bytes (what uniform blocks need). NULL when the
   batch's memory is used up: submit what there is and ask again */
void *zw_gx2_alloc(uint32_t size, uint32_t align);

/* runs the commands on the GPU and waits for them. the color surfaces
   named in readback may then be read on the CPU. 0 when done, negative
   when the batch was dropped (the program is closing) */
int32_t zw_gx2_submit(const ZwGx2Command *commands, uint32_t count, const uint32_t *readback, uint32_t readback_count);

/* main thread (platform/main.c) --------------------------------------- */

/* after SDL has started GX2. loads the shaders; 0 when the GX2 rasterizer
   isn't available, the console then draws on the CPU */
int32_t zw_gx2_init(void);
/* runs a waiting batch, waiting up to timeout_us for one. 1 when it ran one */
int32_t zw_gx2_service(uint32_t timeout_us);
/* drops waiting and later batches, so the console's thread can be stopped */
void zw_gx2_shutdown(void);

#ifdef __cplusplus
}
#endif

#endif
