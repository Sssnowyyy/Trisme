/* the GX2 rasterizer's executor, the C side of zw_gx2.h.

   the console's thread creates surfaces and textures, writes their pixels
   and records commands; zw_gx2_submit hands the batch to the main thread
   and waits. the main thread, the one GX2 belongs to, runs it in
   zw_gx2_service between SDL's frames, under a context state of its own so
   SDL's state is left alone (SDL sets its own state again whenever it draws),
   waits for the GPU, and makes what the GPU wrote visible to the CPU.

   all memory here is MEM2 from the default heap: it survives the HOME menu,
   which takes MEM1 and the foreground bucket away. */

#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <coreinit/cache.h>
#include <coreinit/memdefaultheap.h>
#include <gfd.h>
#include <gx2/clear.h>
#include <gx2/context.h>
#include <gx2/draw.h>
#include <gx2/enum.h>
#include <gx2/event.h>
#include <gx2/mem.h>
#include <gx2/registers.h>
#include <gx2/sampler.h>
#include <gx2/shaders.h>
#include <gx2/state.h>
#include <gx2/surface.h>
#include <gx2/texture.h>
#include <gx2/utils.h>
#include <whb/log.h>

#include "zw_gx2.h"

/* platform/../tools/build-shaders.sh */
typedef struct ZwGx2ShaderBlob {
    const uint8_t *data;
    uint32_t size;
} ZwGx2ShaderBlob;
extern const ZwGx2ShaderBlob zw_gx2_shader_blobs[];
extern const uint32_t zw_gx2_shader_count;

#define RASTER_VARIANTS 8u
#define SHADER_DEPTH_LOAD 8u
#define SHADER_DEPTH_STORE 9u
#define SHADER_COUNT 10u

#define MAX_OBJECTS 2048u
#define RING_SIZE (16u << 20)
#define MIN_ALIGN GX2_UNIFORM_BLOCK_ALIGNMENT

/* ---- objects ---- */

enum { OBJ_FREE = 0, OBJ_COLOR, OBJ_DEPTH, OBJ_TEXTURE };

typedef struct Object {
    uint8_t kind;
    uint8_t written;
    GX2ColorBuffer color;
    GX2DepthBuffer depth;
    /* a texture's own, or a depth buffer's surface seen as a texture */
    GX2Texture texture;
    /* levels a texture couldn't keep, written to and ignored */
    void *spare;
} Object;

static Object objects[MAX_OBJECTS];

static Object *object(uint32_t handle) {
    if (handle == 0 || handle > MAX_OBJECTS) return NULL;
    Object *found = &objects[handle - 1];
    return found->kind == OBJ_FREE ? NULL : found;
}

static uint32_t new_object(uint8_t kind) {
    for (uint32_t i = 0; i < MAX_OBJECTS; i++) {
        if (objects[i].kind == OBJ_FREE) {
            memset(&objects[i], 0, sizeof objects[i]);
            objects[i].kind = kind;
            return i + 1;
        }
    }
    WHBLogPrint("gx2: out of object slots");
    return 0;
}

static void *gpu_alloc(uint32_t size, uint32_t align) {
    if (align < 4) align = 4;
    return MEMAllocFromDefaultHeapEx(size ? size : 4, (int)align);
}

static void gpu_free(void *memory) {
    if (memory) MEMFreeToDefaultHeap(memory);
}

static void init_surface(GX2Surface *surface, uint32_t width, uint32_t height, uint32_t levels, GX2SurfaceFormat format,
                         GX2SurfaceUse use, GX2TileMode tile) {
    memset(surface, 0, sizeof *surface);
    surface->dim = GX2_SURFACE_DIM_TEXTURE_2D;
    surface->width = width;
    surface->height = height;
    surface->depth = 1;
    surface->mipLevels = levels;
    surface->format = format;
    surface->aa = GX2_AA_MODE1X;
    surface->use = use;
    surface->tileMode = tile;
    GX2CalcSurfaceSizeAndAlignment(surface);
}

/* linear-aligned surfaces keep each level's rows 64 pixels apart at least
   (AddrLib: a 256-byte pipe interleave over 4-byte pixels) */
static uint32_t linear_pitch(uint32_t width) {
    return (width + 63u) & ~63u;
}

uint32_t zw_gx2_color_create(uint32_t width, uint32_t height, ZwGx2Level *level) {
    if (width == 0 || height == 0 || width > 8192 || height > 8192) return 0;
    uint32_t handle = new_object(OBJ_COLOR);
    if (!handle) return 0;
    Object *o = &objects[handle - 1];
    GX2ColorBuffer *buffer = &o->color;
    init_surface(&buffer->surface, width, height, 1, GX2_SURFACE_FORMAT_UNORM_R8_G8_B8_A8,
                 (GX2SurfaceUse)(GX2_SURFACE_USE_COLOR_BUFFER | GX2_SURFACE_USE_TEXTURE), GX2_TILE_MODE_LINEAR_ALIGNED);
    buffer->viewMip = 0;
    buffer->viewFirstSlice = 0;
    buffer->viewNumSlices = 1;
    buffer->surface.image = gpu_alloc(buffer->surface.imageSize, buffer->surface.alignment);
    if (!buffer->surface.image) {
        o->kind = OBJ_FREE;
        return 0;
    }
    memset(buffer->surface.image, 0, buffer->surface.imageSize);
    GX2InitColorBufferRegs(buffer);
    o->written = 1;
    level->pixels = buffer->surface.image;
    level->pitch = buffer->surface.pitch;
    level->width = width;
    level->height = height;
    return handle;
}

uint32_t zw_gx2_depth_create(uint32_t width, uint32_t height) {
    if (width == 0 || height == 0 || width > 8192 || height > 8192) return 0;
    uint32_t handle = new_object(OBJ_DEPTH);
    if (!handle) return 0;
    Object *o = &objects[handle - 1];
    GX2DepthBuffer *buffer = &o->depth;
    /* 0x011 is D24S8 to the depth unit and R24X8 to the texture unit,
       which lets the depth be read back through a shader */
    init_surface(&buffer->surface, width, height, 1, GX2_SURFACE_FORMAT_UNORM_R24_X8,
                 (GX2SurfaceUse)(GX2_SURFACE_USE_DEPTH_BUFFER | GX2_SURFACE_USE_TEXTURE), GX2_TILE_MODE_DEFAULT);
    buffer->viewMip = 0;
    buffer->viewFirstSlice = 0;
    buffer->viewNumSlices = 1;
    buffer->hiZPtr = NULL;
    buffer->hiZSize = 0;
    buffer->depthClear = 1.0f;
    buffer->stencilClear = 0;
    buffer->surface.image = gpu_alloc(buffer->surface.imageSize, buffer->surface.alignment);
    if (!buffer->surface.image) {
        o->kind = OBJ_FREE;
        return 0;
    }
    memset(buffer->surface.image, 0, buffer->surface.imageSize);
    DCFlushRange(buffer->surface.image, buffer->surface.imageSize);
    GX2InitDepthBufferRegs(buffer);

    GX2Texture *view = &o->texture;
    memset(view, 0, sizeof *view);
    view->surface = buffer->surface;
    view->viewFirstMip = 0;
    view->viewNumMips = 1;
    view->viewFirstSlice = 0;
    view->viewNumSlices = 1;
    view->compMap = GX2_COMP_MAP(GX2_SQ_SEL_R, GX2_SQ_SEL_R, GX2_SQ_SEL_R, GX2_SQ_SEL_1);
    GX2InitTextureRegs(view);
    o->written = 1;
    return handle;
}

uint32_t zw_gx2_texture_create(uint32_t width, uint32_t height, uint32_t levels, ZwGx2Level *level_out) {
    if (width == 0 || height == 0 || width > 8192 || height > 8192) return 0;
    if (levels == 0) levels = 1;
    if (levels > ZW_GX2_MAX_LEVELS) levels = ZW_GX2_MAX_LEVELS;
    uint32_t handle = new_object(OBJ_TEXTURE);
    if (!handle) return 0;
    Object *o = &objects[handle - 1];
    GX2Texture *texture = &o->texture;
    uint32_t kept = levels;
    for (;;) {
        memset(texture, 0, sizeof *texture);
        init_surface(&texture->surface, width, height, kept, GX2_SURFACE_FORMAT_UNORM_R8_G8_B8_A8, GX2_SURFACE_USE_TEXTURE,
                     GX2_TILE_MODE_LINEAR_ALIGNED);
        /* the levels where the CPU expects them, or fewer levels */
        int ok = texture->surface.pitch == linear_pitch(width);
        for (uint32_t i = 1; ok && i < kept; i++) {
            uint32_t w = width >> i ? width >> i : 1, h = height >> i ? height >> i : 1;
            uint32_t offset = i == 1 ? 0 : texture->surface.mipLevelOffset[i - 1];
            ok = offset + linear_pitch(w) * h * 4 <= texture->surface.mipmapSize;
        }
        if (ok || kept == 1) break;
        kept = 1;
    }
    texture->viewFirstMip = 0;
    texture->viewNumMips = kept;
    texture->viewFirstSlice = 0;
    texture->viewNumSlices = 1;
    texture->compMap = GX2_COMP_MAP(GX2_SQ_SEL_R, GX2_SQ_SEL_G, GX2_SQ_SEL_B, GX2_SQ_SEL_A);
    texture->surface.image = gpu_alloc(texture->surface.imageSize, texture->surface.alignment);
    if (kept > 1) texture->surface.mipmaps = gpu_alloc(texture->surface.mipmapSize, texture->surface.alignment);
    uint32_t spare_size = 0;
    if (kept < levels) spare_size = linear_pitch(width >> 1 ? width >> 1 : 1) * (height >> 1 ? height >> 1 : 1) * 4;
    if (spare_size) o->spare = malloc(spare_size);
    if (!texture->surface.image || (kept > 1 && !texture->surface.mipmaps) || (spare_size && !o->spare)) {
        gpu_free(texture->surface.image);
        gpu_free(texture->surface.mipmaps);
        free(o->spare);
        o->kind = OBJ_FREE;
        return 0;
    }
    GX2InitTextureRegs(texture);
    for (uint32_t i = 0; i < levels; i++) {
        uint32_t w = width >> i ? width >> i : 1, h = height >> i ? height >> i : 1;
        level_out[i].width = w;
        level_out[i].height = h;
        level_out[i].pitch = i == 0 ? texture->surface.pitch : linear_pitch(w);
        if (i == 0) {
            level_out[i].pixels = texture->surface.image;
        } else if (i < kept) {
            level_out[i].pixels = (uint8_t *)texture->surface.mipmaps + (i == 1 ? 0 : texture->surface.mipLevelOffset[i - 1]);
        } else {
            level_out[i].pixels = o->spare;
        }
    }
    o->written = 1;
    return handle;
}

void zw_gx2_written(uint32_t handle) {
    Object *o = object(handle);
    if (o) o->written = 1;
}

void zw_gx2_destroy(uint32_t handle) {
    Object *o = object(handle);
    if (!o) return;
    switch (o->kind) {
    case OBJ_COLOR:
        gpu_free(o->color.surface.image);
        break;
    case OBJ_DEPTH:
        gpu_free(o->depth.surface.image);
        break;
    case OBJ_TEXTURE:
        gpu_free(o->texture.surface.image);
        gpu_free(o->texture.surface.mipmaps);
        free(o->spare);
        break;
    }
    memset(o, 0, sizeof *o);
}

/* makes what the CPU wrote visible to the GPU, main thread, in a batch */
static void publish(Object *o) {
    if (!o->written) return;
    o->written = 0;
    GX2Surface *surface = o->kind == OBJ_COLOR ? &o->color.surface : o->kind == OBJ_DEPTH ? &o->depth.surface : &o->texture.surface;
    DCFlushRange(surface->image, surface->imageSize);
    GX2Invalidate(GX2_INVALIDATE_MODE_TEXTURE | GX2_INVALIDATE_MODE_COLOR_BUFFER | GX2_INVALIDATE_MODE_DEPTH_BUFFER,
                  surface->image, surface->imageSize);
    if (surface->mipmaps && surface->mipmapSize) {
        DCFlushRange(surface->mipmaps, surface->mipmapSize);
        GX2Invalidate(GX2_INVALIDATE_MODE_TEXTURE, surface->mipmaps, surface->mipmapSize);
    }
}

/* ---- the batch's memory ---- */

static uint8_t *ring;
static uint32_t ring_used;

void *zw_gx2_alloc(uint32_t size, uint32_t align) {
    if (!ring) return NULL;
    if (align < MIN_ALIGN) align = MIN_ALIGN;
    uint32_t at = (ring_used + align - 1) & ~(align - 1);
    if (at > RING_SIZE || size > RING_SIZE - at) return NULL;
    ring_used = at + size;
    return ring + at;
}

/* ---- shaders ---- */

typedef struct Program {
    GX2VertexShader *vertex;
    GX2PixelShader *pixel;
    GX2FetchShader fetch;
    void *fetch_program;
    /* where Draw and Tables go, -1 when the shader doesn't read them */
    int32_t draw_block, tables_block;
    /* the sampler location of texture0..2 (depth_texels or depth as 0) */
    int32_t samplers[3];
} Program;

static Program programs[SHADER_COUNT];
static GX2ContextState *context;
static float *fullscreen_triangle;
/* read by the lighting and procedural texture variants when a draw passes
   no tables, so they never read a batch's memory that has been reused */
static void *zero_tables;
static uint32_t white_texture;
static ZwGx2Level white_level;

static GX2VertexShader *load_vertex(const void *file) {
    uint32_t header = GFDGetVertexShaderHeaderSize(0, file), size = GFDGetVertexShaderProgramSize(0, file);
    if (!header || !size) return NULL;
    GX2VertexShader *shader = calloc(1, header);
    void *program = gpu_alloc(size, GX2_SHADER_PROGRAM_ALIGNMENT);
    if (!shader || !program || !GFDGetVertexShader(shader, program, 0, file)) {
        free(shader);
        gpu_free(program);
        return NULL;
    }
    GX2Invalidate(GX2_INVALIDATE_MODE_CPU_SHADER, shader->program, shader->size);
    return shader;
}

static GX2PixelShader *load_pixel(const void *file) {
    uint32_t header = GFDGetPixelShaderHeaderSize(0, file), size = GFDGetPixelShaderProgramSize(0, file);
    if (!header || !size) return NULL;
    GX2PixelShader *shader = calloc(1, header);
    void *program = gpu_alloc(size, GX2_SHADER_PROGRAM_ALIGNMENT);
    if (!shader || !program || !GFDGetPixelShader(shader, program, 0, file)) {
        free(shader);
        gpu_free(program);
        return NULL;
    }
    GX2Invalidate(GX2_INVALIDATE_MODE_CPU_SHADER, shader->program, shader->size);
    return shader;
}

static int32_t pixel_block(const GX2PixelShader *shader, const char *name) {
    for (uint32_t i = 0; i < shader->uniformBlockCount; i++) {
        if (strcmp(shader->uniformBlocks[i].name, name) == 0) return (int32_t)shader->uniformBlocks[i].offset;
    }
    return -1;
}

static int32_t pixel_sampler(const GX2PixelShader *shader, const char *name) {
    for (uint32_t i = 0; i < shader->samplerVarCount; i++) {
        if (strcmp(shader->samplerVars[i].name, name) == 0) return (int32_t)shader->samplerVars[i].location;
    }
    return -1;
}

static int32_t vertex_attribute(const GX2VertexShader *shader, const char *name) {
    for (uint32_t i = 0; i < shader->attribVarCount; i++) {
        if (strcmp(shader->attribVars[i].name, name) == 0) return (int32_t)shader->attribVars[i].location;
    }
    return -1;
}

/* a fetch shader for the vertex shader's attributes out of one buffer */
static int make_fetch(Program *program, const char *const *names, uint32_t count, uint32_t stride_floats) {
    GX2AttribStream streams[8];
    uint32_t used = 0;
    for (uint32_t i = 0; i < count; i++) {
        int32_t location = vertex_attribute(program->vertex, names[i]);
        if (location < 0) continue; /* compiled away */
        GX2AttribStream *stream = &streams[used++];
        memset(stream, 0, sizeof *stream);
        stream->location = (uint32_t)location;
        stream->buffer = 0;
        stream->offset = i * 16;
        stream->format = stride_floats == 2 ? GX2_ATTRIB_FORMAT_FLOAT_32_32 : GX2_ATTRIB_FORMAT_FLOAT_32_32_32_32;
        stream->type = GX2_ATTRIB_INDEX_PER_VERTEX;
        stream->aluDivisor = 0;
        stream->mask = stride_floats == 2 ? GX2_SEL_MASK(GX2_SQ_SEL_X, GX2_SQ_SEL_Y, GX2_SQ_SEL_0, GX2_SQ_SEL_1)
                                          : GX2_SEL_MASK(GX2_SQ_SEL_X, GX2_SQ_SEL_Y, GX2_SQ_SEL_Z, GX2_SQ_SEL_W);
        /* the CPU writes its floats big-endian, which the default swap reads */
        stream->endianSwap = GX2_ENDIAN_SWAP_DEFAULT;
    }
    uint32_t size = GX2CalcFetchShaderSizeEx(used, GX2_FETCH_SHADER_TESSELLATION_NONE, GX2_TESSELLATION_MODE_DISCRETE);
    program->fetch_program = gpu_alloc(size, GX2_SHADER_PROGRAM_ALIGNMENT);
    if (!program->fetch_program) return -1;
    GX2InitFetchShaderEx(&program->fetch, program->fetch_program, used, streams, GX2_FETCH_SHADER_TESSELLATION_NONE,
                         GX2_TESSELLATION_MODE_DISCRETE);
    GX2Invalidate(GX2_INVALIDATE_MODE_CPU_SHADER, program->fetch_program, size);
    return 0;
}

static int load_program(uint32_t index) {
    const ZwGx2ShaderBlob *blob = &zw_gx2_shader_blobs[index];
    Program *program = &programs[index];
    program->vertex = load_vertex(blob->data);
    program->pixel = load_pixel(blob->data);
    if (!program->vertex || !program->pixel) {
        WHBLogPrintf("gx2: shader %u didn't load: %s", index, GFDGetLastErrorString());
        return -1;
    }
    static const char *const raster_inputs[] = {"position", "color", "texcoords01", "texcoord2_depth", "quaternion", "view"};
    static const char *const fullscreen_inputs[] = {"position"};
    if (index < RASTER_VARIANTS) {
        if (make_fetch(program, raster_inputs, 6, 4) != 0) return -1;
        program->draw_block = pixel_block(program->pixel, "Draw");
        program->tables_block = pixel_block(program->pixel, "Tables");
        program->samplers[0] = pixel_sampler(program->pixel, "texture0");
        program->samplers[1] = pixel_sampler(program->pixel, "texture1");
        program->samplers[2] = pixel_sampler(program->pixel, "texture2");
    } else {
        if (make_fetch(program, fullscreen_inputs, 1, 2) != 0) return -1;
        program->draw_block = program->tables_block = -1;
        program->samplers[0] = pixel_sampler(program->pixel, index == SHADER_DEPTH_LOAD ? "depth_texels" : "depth");
        program->samplers[1] = program->samplers[2] = -1;
    }
    return 0;
}

/* ---- PICA -> GX2 ---- */

static const GX2CompareFunction compares[8] = {
    GX2_COMPARE_FUNC_NEVER, GX2_COMPARE_FUNC_ALWAYS, GX2_COMPARE_FUNC_EQUAL,   GX2_COMPARE_FUNC_NOT_EQUAL,
    GX2_COMPARE_FUNC_LESS,  GX2_COMPARE_FUNC_LEQUAL, GX2_COMPARE_FUNC_GREATER, GX2_COMPARE_FUNC_GEQUAL,
};

/* the PICA's stencil operations are in GX2's order */
static GX2StencilFunction stencil_op(uint8_t op) {
    return (GX2StencilFunction)(op & 7);
}

static const GX2BlendCombineMode combines[5] = {
    GX2_BLEND_COMBINE_MODE_ADD, GX2_BLEND_COMBINE_MODE_SUB, GX2_BLEND_COMBINE_MODE_REV_SUB,
    GX2_BLEND_COMBINE_MODE_MIN, GX2_BLEND_COMBINE_MODE_MAX,
};

static const GX2BlendMode factors[15] = {
    GX2_BLEND_MODE_ZERO,          GX2_BLEND_MODE_ONE,           GX2_BLEND_MODE_SRC_COLOR,
    GX2_BLEND_MODE_INV_SRC_COLOR, GX2_BLEND_MODE_DST_COLOR,     GX2_BLEND_MODE_INV_DST_COLOR,
    GX2_BLEND_MODE_SRC_ALPHA,     GX2_BLEND_MODE_INV_SRC_ALPHA, GX2_BLEND_MODE_DST_ALPHA,
    GX2_BLEND_MODE_INV_DST_ALPHA, GX2_BLEND_MODE_BLEND_FACTOR,  GX2_BLEND_MODE_INV_BLEND_FACTOR,
    GX2_BLEND_MODE_CONSTANT_ALPHA, GX2_BLEND_MODE_INV_CONSTANT_ALPHA, GX2_BLEND_MODE_SRC_ALPHA_SAT,
};

/* the PICA's logic operations as ROP3 codes (source 0xCC, destination 0xAA) */
static const GX2LogicOp logic_ops[16] = {
    GX2_LOGIC_OP_CLEAR, GX2_LOGIC_OP_AND,     GX2_LOGIC_OP_REV_AND, GX2_LOGIC_OP_COPY,
    GX2_LOGIC_OP_SET,   GX2_LOGIC_OP_INV_COPY, GX2_LOGIC_OP_NOP,    GX2_LOGIC_OP_INV,
    GX2_LOGIC_OP_NOT_AND, GX2_LOGIC_OP_OR,    GX2_LOGIC_OP_NOR,     GX2_LOGIC_OP_XOR,
    GX2_LOGIC_OP_EQUIV, GX2_LOGIC_OP_INV_AND, GX2_LOGIC_OP_REV_OR,  GX2_LOGIC_OP_INV_OR,
};

static GX2BlendMode factor(uint8_t value) {
    return factors[value < 15 ? value : 1];
}

static GX2BlendCombineMode combine(uint8_t value) {
    return combines[value < 5 ? value : 0];
}

static GX2TexClampMode clamp_mode(uint8_t wrap) {
    switch (wrap) {
    case 2:
        return GX2_TEX_CLAMP_MODE_WRAP;
    case 3:
        return GX2_TEX_CLAMP_MODE_MIRROR;
    default:
        /* the border the shader draws itself */
        return GX2_TEX_CLAMP_MODE_CLAMP;
    }
}

/* ---- running a batch, main thread ---- */

static uint32_t target_width, target_height;

static void set_targets(Object *color, Object *depth) {
    if (color) {
        GX2SetColorBuffer(&color->color, GX2_RENDER_TARGET_0);
        target_width = color->color.surface.width;
        target_height = color->color.surface.height;
    }
    if (depth) {
        GX2SetDepthBuffer(&depth->depth);
        if (!color) {
            target_width = depth->depth.surface.width;
            target_height = depth->depth.surface.height;
        }
    }
    GX2SetViewport(0.0f, 0.0f, (float)target_width, (float)target_height, 0.0f, 1.0f);
}

static void set_scissor(const int32_t scissor[4]) {
    int32_t x0 = scissor[0] < 0 ? 0 : scissor[0], y0 = scissor[1] < 0 ? 0 : scissor[1];
    int32_t x1 = scissor[2] > (int32_t)target_width ? (int32_t)target_width : scissor[2];
    int32_t y1 = scissor[3] > (int32_t)target_height ? (int32_t)target_height : scissor[3];
    if (x1 <= x0 || y1 <= y0) {
        GX2SetScissor(0, 0, 0, 0);
        return;
    }
    GX2SetScissor((uint32_t)x0, (uint32_t)y0, (uint32_t)(x1 - x0), (uint32_t)(y1 - y0));
}

/* what every draw of ours wants, whatever the last one left */
static void common_state(void) {
    GX2SetShaderMode(GX2_SHADER_MODE_UNIFORM_BLOCK);
    GX2SetRasterizerClipControlHalfZ(TRUE, TRUE, TRUE);
    GX2SetCullOnlyControl(GX2_FRONT_FACE_CCW, FALSE, FALSE);
    GX2SetAlphaTest(FALSE, GX2_COMPARE_FUNC_ALWAYS, 0.0f);
}

static void bind_texture(const Program *program, uint32_t unit, const GX2Texture *texture, const ZwGx2Sampler *setting) {
    if (program->samplers[unit] < 0) return;
    GX2Sampler sampler;
    GX2TexClampMode s = setting ? clamp_mode(setting->wrap_s) : GX2_TEX_CLAMP_MODE_CLAMP;
    GX2TexClampMode t = setting ? clamp_mode(setting->wrap_t) : GX2_TEX_CLAMP_MODE_CLAMP;
    GX2TexXYFilterMode mag = setting && setting->mag_linear ? GX2_TEX_XY_FILTER_MODE_LINEAR : GX2_TEX_XY_FILTER_MODE_POINT;
    GX2TexXYFilterMode min = setting && setting->min_linear ? GX2_TEX_XY_FILTER_MODE_LINEAR : GX2_TEX_XY_FILTER_MODE_POINT;
    GX2TexMipFilterMode mip = GX2_TEX_MIP_FILTER_MODE_NONE;
    if (setting && texture->viewNumMips > 1) {
        mip = setting->mip == 2 ? GX2_TEX_MIP_FILTER_MODE_LINEAR : setting->mip == 1 ? GX2_TEX_MIP_FILTER_MODE_POINT : GX2_TEX_MIP_FILTER_MODE_NONE;
    }
    GX2InitSampler(&sampler, s, mag);
    GX2InitSamplerClamping(&sampler, s, t, GX2_TEX_CLAMP_MODE_CLAMP);
    GX2InitSamplerXYFilter(&sampler, mag, min, GX2_TEX_ANISO_RATIO_NONE);
    GX2InitSamplerZMFilter(&sampler, GX2_TEX_Z_FILTER_MODE_NONE, mip);
    if (setting) {
        float most = (float)(texture->viewNumMips - 1);
        float max_lod = setting->max_lod < most ? setting->max_lod : most;
        float min_lod = setting->min_lod < max_lod ? setting->min_lod : max_lod;
        GX2InitSamplerLOD(&sampler, min_lod < 0.0f ? 0.0f : min_lod, max_lod < 0.0f ? 0.0f : max_lod, setting->lod_bias);
    } else {
        GX2InitSamplerLOD(&sampler, 0.0f, 0.0f, 0.0f);
    }
    GX2SetPixelTexture(texture, (uint32_t)program->samplers[unit]);
    GX2SetPixelSampler(&sampler, (uint32_t)program->samplers[unit]);
}

static void use_program(const Program *program) {
    GX2SetFetchShader(&program->fetch);
    GX2SetVertexShader(program->vertex);
    GX2SetPixelShader(program->pixel);
}

static void run_draw(const ZwGx2Command *command) {
    Object *color = object(command->color);
    Object *depth = object(command->depth);
    if (!color || !command->vertices || command->vertex_count == 0 || !command->uniforms) return;
    const Program *program = &programs[command->shader & (RASTER_VARIANTS - 1)];
    if (!program->vertex) return;
    common_state();
    set_targets(color, depth);
    set_scissor(command->scissor);

    /* depth and stencil: the PICA writes depth only where it tests it, and
       tests only with a depth buffer */
    if (depth) {
        BOOL test = command->depth_test || command->depth_write;
        GX2CompareFunction depth_func = command->depth_test ? compares[command->depth_func & 7] : GX2_COMPARE_FUNC_ALWAYS;
        GX2CompareFunction stencil_func = compares[command->stencil_func & 7];
        GX2StencilFunction fail = stencil_op(command->stencil_fail), zfail = stencil_op(command->stencil_zfail),
                           zpass = stencil_op(command->stencil_zpass);
        GX2SetDepthStencilControl(test, command->depth_write ? TRUE : FALSE, depth_func, command->stencil_test ? TRUE : FALSE, FALSE,
                                  stencil_func, zpass, zfail, fail, stencil_func, zpass, zfail, fail);
        GX2SetStencilMask(command->stencil_mask, command->stencil_write_mask, command->stencil_ref, command->stencil_mask,
                          command->stencil_write_mask, command->stencil_ref);
    } else {
        GX2SetDepthStencilControl(FALSE, FALSE, GX2_COMPARE_FUNC_ALWAYS, FALSE, FALSE, GX2_COMPARE_FUNC_ALWAYS,
                                  GX2_STENCIL_FUNCTION_KEEP, GX2_STENCIL_FUNCTION_KEEP, GX2_STENCIL_FUNCTION_KEEP,
                                  GX2_COMPARE_FUNC_ALWAYS, GX2_STENCIL_FUNCTION_KEEP, GX2_STENCIL_FUNCTION_KEEP,
                                  GX2_STENCIL_FUNCTION_KEEP);
    }

    /* blending or a logic operation, never both on the PICA */
    uint8_t mask = command->color_mask & 0xF;
    GX2SetTargetChannelMasks((GX2ChannelMask)mask, 0, 0, 0, 0, 0, 0, 0);
    if (command->logic_op_enable && !command->blend_enable) {
        GX2SetColorControl(logic_ops[command->logic_op & 15], 0, FALSE, mask ? TRUE : FALSE);
    } else {
        GX2SetColorControl(GX2_LOGIC_OP_COPY, command->blend_enable ? 1 : 0, FALSE, mask ? TRUE : FALSE);
    }
    if (command->blend_enable) {
        GX2SetBlendControl(GX2_RENDER_TARGET_0, factor(command->blend_src_rgb), factor(command->blend_dst_rgb),
                           combine(command->blend_eq_rgb), TRUE, factor(command->blend_src_alpha), factor(command->blend_dst_alpha),
                           combine(command->blend_eq_alpha));
        uint32_t c = command->blend_color;
        GX2SetBlendConstantColor((float)(c & 0xFF) / 255.0f, (float)((c >> 8) & 0xFF) / 255.0f, (float)((c >> 16) & 0xFF) / 255.0f,
                                 (float)(c >> 24) / 255.0f);
    }

    use_program(program);
    if (program->draw_block >= 0) GX2SetPixelUniformBlock((uint32_t)program->draw_block, command->uniforms_size, command->uniforms);
    if (program->tables_block >= 0) {
        GX2SetPixelUniformBlock((uint32_t)program->tables_block, ZW_GX2_TABLES_SIZE, command->tables ? command->tables : zero_tables);
    }
    for (uint32_t unit = 0; unit < 3; unit++) {
        Object *texture = object(command->textures[unit]);
        if (texture && texture->kind == OBJ_TEXTURE) {
            bind_texture(program, unit, &texture->texture, &command->samplers[unit]);
        } else if (texture && texture->kind == OBJ_COLOR) {
            /* never handed out as a texture, so not expected; draw white */
            bind_texture(program, unit, &objects[white_texture - 1].texture, NULL);
        } else {
            bind_texture(program, unit, &objects[white_texture - 1].texture, NULL);
        }
    }

    GX2SetAttribBuffer(0, command->vertex_count * ZW_GX2_VERTEX_SIZE, ZW_GX2_VERTEX_SIZE, command->vertices);
    if (command->indices && command->index_count) {
        GX2DrawIndexedEx(GX2_PRIMITIVE_MODE_TRIANGLES, command->index_count, GX2_INDEX_TYPE_U32, command->indices, 0, 1);
    } else {
        GX2DrawEx(GX2_PRIMITIVE_MODE_TRIANGLES, command->vertex_count - command->vertex_count % 3, 0, 1);
    }
}

static void run_fullscreen(const Program *program, const GX2Texture *source) {
    use_program(program);
    bind_texture(program, 0, source, NULL);
    GX2SetAttribBuffer(0, 3 * 2 * sizeof(float), 2 * sizeof(float), fullscreen_triangle);
    GX2DrawEx(GX2_PRIMITIVE_MODE_TRIANGLES, 3, 0, 1);
}

static void run_load_depth(const ZwGx2Command *command) {
    Object *depth = object(command->depth);
    Object *source = object(command->textures[0]);
    const Program *program = &programs[SHADER_DEPTH_LOAD];
    if (!depth || !source || source->kind != OBJ_TEXTURE || !program->vertex) return;
    common_state();
    set_targets(NULL, depth);
    GX2SetScissor(0, 0, target_width, target_height);
    /* depth alone: no color is written, whatever color buffer is bound */
    GX2SetTargetChannelMasks((GX2ChannelMask)0, 0, 0, 0, 0, 0, 0, 0);
    GX2SetColorControl(GX2_LOGIC_OP_COPY, 0, FALSE, FALSE);
    GX2SetDepthStencilControl(TRUE, TRUE, GX2_COMPARE_FUNC_ALWAYS, FALSE, FALSE, GX2_COMPARE_FUNC_ALWAYS, GX2_STENCIL_FUNCTION_KEEP,
                              GX2_STENCIL_FUNCTION_KEEP, GX2_STENCIL_FUNCTION_KEEP, GX2_COMPARE_FUNC_ALWAYS, GX2_STENCIL_FUNCTION_KEEP,
                              GX2_STENCIL_FUNCTION_KEEP, GX2_STENCIL_FUNCTION_KEEP);
    run_fullscreen(program, &source->texture);
}

static void run_store_depth(const ZwGx2Command *command) {
    Object *depth = object(command->depth);
    Object *color = object(command->color);
    const Program *program = &programs[SHADER_DEPTH_STORE];
    if (!depth || !color || color->kind != OBJ_COLOR || !program->vertex) return;
    /* what the depth unit wrote, out of its cache, for the texture unit */
    GX2Invalidate(GX2_INVALIDATE_MODE_DEPTH_BUFFER | GX2_INVALIDATE_MODE_TEXTURE, depth->depth.surface.image, depth->depth.surface.imageSize);
    common_state();
    set_targets(color, NULL);
    GX2SetScissor(0, 0, target_width, target_height);
    GX2SetTargetChannelMasks(GX2_CHANNEL_MASK_RGBA, 0, 0, 0, 0, 0, 0, 0);
    GX2SetColorControl(GX2_LOGIC_OP_COPY, 0, FALSE, TRUE);
    GX2SetDepthOnlyControl(FALSE, FALSE, GX2_COMPARE_FUNC_ALWAYS);
    run_fullscreen(program, &depth->texture);
}

static void run_batch(const ZwGx2Command *commands, uint32_t count, const uint32_t *readback, uint32_t readback_count) {
    GX2SetContextState(context);
    for (uint32_t i = 0; i < MAX_OBJECTS; i++) {
        if (objects[i].kind != OBJ_FREE && objects[i].written) publish(&objects[i]);
    }
    if (ring_used) {
        DCFlushRange(ring, ring_used);
        GX2Invalidate(GX2_INVALIDATE_MODE_ATTRIBUTE_BUFFER | GX2_INVALIDATE_MODE_UNIFORM_BLOCK, ring, ring_used);
    }
    for (uint32_t i = 0; i < count; i++) {
        const ZwGx2Command *command = &commands[i];
        switch (command->kind) {
        case ZW_GX2_CMD_DRAW:
            run_draw(command);
            break;
        case ZW_GX2_CMD_CLEAR_COLOR: {
            Object *color = object(command->color);
            if (color && color->kind == OBJ_COLOR) {
                GX2ClearColor(&color->color, command->clear_color[0], command->clear_color[1], command->clear_color[2],
                              command->clear_color[3]);
                /* clears leave GX2's state their own way */
                GX2SetContextState(context);
            }
            break;
        }
        case ZW_GX2_CMD_CLEAR_DEPTH: {
            Object *depth = object(command->depth);
            uint32_t flags = (command->clear_flags & 1 ? GX2_CLEAR_FLAGS_DEPTH : 0) | (command->clear_flags & 2 ? GX2_CLEAR_FLAGS_STENCIL : 0);
            if (depth && depth->kind == OBJ_DEPTH && flags) {
                GX2ClearDepthStencilEx(&depth->depth, command->clear_depth, command->clear_stencil, (GX2ClearFlags)flags);
                GX2SetContextState(context);
            }
            break;
        }
        case ZW_GX2_CMD_LOAD_DEPTH:
            run_load_depth(command);
            break;
        case ZW_GX2_CMD_STORE_DEPTH:
            run_store_depth(command);
            break;
        }
    }
    for (uint32_t i = 0; i < readback_count; i++) {
        Object *color = object(readback[i]);
        if (color && color->kind == OBJ_COLOR) {
            GX2Invalidate(GX2_INVALIDATE_MODE_COLOR_BUFFER, color->color.surface.image, color->color.surface.imageSize);
        }
    }
    GX2DrawDone();
    for (uint32_t i = 0; i < readback_count; i++) {
        Object *color = object(readback[i]);
        if (color && color->kind == OBJ_COLOR) DCInvalidateRange(color->color.surface.image, color->color.surface.imageSize);
    }
}

/* ---- handing batches over ---- */

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int available, closing;
static struct {
    const ZwGx2Command *commands;
    uint32_t count;
    const uint32_t *readback;
    uint32_t readback_count;
    int pending, done;
} job;

int32_t zw_gx2_available(void) {
    pthread_mutex_lock(&lock);
    int32_t result = available && !closing;
    pthread_mutex_unlock(&lock);
    return result;
}

int32_t zw_gx2_submit(const ZwGx2Command *commands, uint32_t count, const uint32_t *readback, uint32_t readback_count) {
    pthread_mutex_lock(&lock);
    if (!available || closing) {
        ring_used = 0;
        pthread_mutex_unlock(&lock);
        return -1;
    }
    job.commands = commands;
    job.count = count;
    job.readback = readback;
    job.readback_count = readback_count;
    job.done = 0;
    job.pending = 1;
    pthread_cond_broadcast(&changed);
    while (!job.done && !closing) pthread_cond_wait(&changed, &lock);
    int32_t result = job.done ? 0 : -1;
    job.pending = 0;
    ring_used = 0;
    pthread_mutex_unlock(&lock);
    return result;
}

int32_t zw_gx2_service(uint32_t timeout_us) {
    pthread_mutex_lock(&lock);
    if (!available || closing) {
        pthread_mutex_unlock(&lock);
        return 0;
    }
    if (!job.pending || job.done) {
        if (timeout_us == 0) {
            pthread_mutex_unlock(&lock);
            return 0;
        }
        struct timespec until;
        clock_gettime(CLOCK_REALTIME, &until);
        until.tv_nsec += (long)(timeout_us % 1000000u) * 1000;
        until.tv_sec += timeout_us / 1000000u + until.tv_nsec / 1000000000;
        until.tv_nsec %= 1000000000;
        while ((!job.pending || job.done) && !closing) {
            if (pthread_cond_timedwait(&changed, &lock, &until) != 0) break;
        }
        if (!job.pending || job.done || closing) {
            pthread_mutex_unlock(&lock);
            return 0;
        }
    }
    /* the console's thread waits on this batch and touches none of it */
    pthread_mutex_unlock(&lock);
    run_batch(job.commands, job.count, job.readback, job.readback_count);
    pthread_mutex_lock(&lock);
    job.done = 1;
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&lock);
    return 1;
}

void zw_gx2_shutdown(void) {
    pthread_mutex_lock(&lock);
    closing = 1;
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&lock);
}

int32_t zw_gx2_init(void) {
    if (zw_gx2_shader_count < SHADER_COUNT) {
        WHBLogPrint("gx2: the program has no GX2 shaders (the kit was built without CafeGLSL); 3D is drawn on the CPU");
        return 0;
    }
    ring = gpu_alloc(RING_SIZE, GX2_VERTEX_BUFFER_ALIGNMENT > MIN_ALIGN ? GX2_VERTEX_BUFFER_ALIGNMENT : MIN_ALIGN);
    context = gpu_alloc(sizeof(GX2ContextState), GX2_CONTEXT_STATE_ALIGNMENT);
    fullscreen_triangle = gpu_alloc(6 * sizeof(float), GX2_VERTEX_BUFFER_ALIGNMENT);
    zero_tables = gpu_alloc(ZW_GX2_TABLES_SIZE, GX2_UNIFORM_BLOCK_ALIGNMENT);
    if (!ring || !context || !fullscreen_triangle || !zero_tables) {
        WHBLogPrint("gx2: out of memory");
        return 0;
    }
    memset(zero_tables, 0, ZW_GX2_TABLES_SIZE);
    DCFlushRange(zero_tables, ZW_GX2_TABLES_SIZE);
    GX2Invalidate(GX2_INVALIDATE_MODE_UNIFORM_BLOCK, zero_tables, ZW_GX2_TABLES_SIZE);
    static const float corners[6] = {-1.0f, -1.0f, 3.0f, -1.0f, -1.0f, 3.0f};
    memcpy(fullscreen_triangle, corners, sizeof corners);
    DCFlushRange(fullscreen_triangle, sizeof corners);
    GX2Invalidate(GX2_INVALIDATE_MODE_CPU_ATTRIBUTE_BUFFER, fullscreen_triangle, sizeof corners);

    for (uint32_t i = 0; i < SHADER_COUNT; i++) {
        if (load_program(i) != 0) return 0;
    }
    white_texture = zw_gx2_texture_create(1, 1, 1, &white_level);
    if (!white_texture) return 0;
    memset(white_level.pixels, 0xFF, 4);

    /* our own state; SDL sets its own again whenever it draws */
    GX2SetupContextStateEx(context, FALSE);
    common_state();

    pthread_mutex_lock(&lock);
    available = 1;
    pthread_mutex_unlock(&lock);
    WHBLogPrint("gx2: the 3D is drawn on the GPU");
    return 1;
}
