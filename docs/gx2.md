# The GX2 renderer

By default a converted game draws its 3D on the Wii U's GPU (Latte) through GX2, and falls back to Zakuro's software rasterizer on the CPU when it can't.

## What runs where

| Work | Where |
|---|---|
| The game's ARM code (recompiled), the HLE kernel and services | CPU core 2, the console's thread |
| The game's vertex shaders, clipping, culling | CPU core 2 (Zakuro's shader interpreter) |
| Rasterization, texturing, fragment lighting, the six combiners, fog, alpha/stencil/depth tests, blending and logic ops | GPU |
| Display transfers, memory fills, presenting the screens | CPU, then SDL on the GPU |

Fragment work is what costs most on the CPU, so this is where the GPU helps. The vertex stage stays on the CPU because the Vulkan backend's GPU vertex path interprets PICA shader programs in a shader, which is too heavy for Latte.

## How it fits together

- **`zakuro/crates/zakuro-gpu/src/raster/gx2.rs`** (Rust, the `gx2` feature) is a backend in the same shape as Zakuro's Vulkan one, `raster/hardware.rs`. It keeps a cache of the console's color and depth buffers and textures, fills each draw's uniform block the way the Vulkan backend does, and records commands. Guest memory stays the source of truth: buffers go up to the GPU before they're drawn into, and come back down into guest memory, re-tiled in the guest's pixel format, before anything else reads them (a display transfer, a texture, a fill, the CPU).
- **`platform/zw_gx2.h`** is the interface between the two halves: plain structs and handles, no GX2 types.
- **`platform/gx2.c`** (C) is the executor. GX2 belongs to the core that called `GX2Init`, the main thread's, which SDL uses too. So the console's thread never calls GX2: it submits a batch, and the main thread runs it between SDL's frames under its own `GX2ContextState`, waits for the GPU, and hands it back. A submit blocks until then, which keeps the hand-off simple and safe; the cost is that the console's thread waits while the GPU draws.
- **Shaders** are compiled ahead of time with [CafeGLSL](https://github.com/Exzap/CafeGLSL), so no compiler ships in the game:
  - `platform/shaders/raster.frag` is generated from Zakuro's Vulkan ubershader by `tools/gx2_raster_frag.py`. Specialization constants become `DYNAMIC = 1` (combiners read from the uniform block), and the tables' storage buffer becomes a uniform block holding the same bytes. Run the script again when Zakuro's shader changes.
  - 8 variants: fragment lighting on/off × procedural texture on/off × writes depth (w-buffer) or not.
  - `raster.vert` passes the CPU-shaded vertices through, `fullscreen.vert` with `depth_load.frag` / `depth_store.frag` move depth in and out of the GPU.
  - `tools/build-shaders.sh` compiles them and embeds the `.gsh` files in `kit/obj/gx2_shaders.o`.
- All GX2 memory is MEM2, so it survives the HOME menu. While the HOME menu is up, a batch the console submitted waits; the console is paused then anyway.

## Building with it

Download CafeGLSL v1.0.0-rc1 or newer for your computer (`cafeglsl-*-linux-x86_64`, `-macos-arm64`, `-macos-x86_64` or `-windows-x86_64`; not the `wiiu-ppc` one). It's a pre-release, so the releases page's "Latest" label points at v0.2.0 instead, which has only a Linux compiler. Unpack it (on macOS, `xattr -dr com.apple.quarantine <folder>` if it won't run) and point the kit build at it:

```
CAFEGLSL_DIR=/path/to/cafeglsl-1.0.0-rc1-macos-arm64 tools/build-kit.sh
```

`CAFEGLSL=/path/to/glslcompiler` works too, as does having `glslcompiler` on `PATH`. Without it the kit still builds, prints a warning, and games draw on the CPU. `ZW_REQUIRE_GX2=1` turns the warning into an error.

## Choosing the renderer

Each game's `sd:/wiiu/zakuro/<title id>/wiiu.cfg` has a `renderer=` line: `1` for the GPU (the default), `0` for the CPU. It's read when the game starts. The file is written the first time you change layout; you can also create it by hand.

The log (`WHBLog`, over UDP or Cafe) says which renderer is used and why the GPU one isn't, when it isn't.

## Limits

- **Stencil** can't be read back from the GPU into guest memory, or written from guest memory into the GPU: Latte's shaders can't export stencil. Games that clear stencil with a memory fill are fine (fills become GPU clears), but a game that reads stencil on the CPU sees stale data.
- **Depth** goes back to guest memory only when something reads it, through a shader that packs it into a color buffer.
- **Partial memory fills** over a buffer the GPU drew in make it write the buffer back first when the drawn rows reach past the fill (GX2 clears have no row range). Whole-buffer fills become GPU clears.
- **Resolution** is native (400×240 and 320×240). No upscaling yet.
- **Not on the GPU:** display transfers (done on the CPU after a write-back), texture packs, frame interpolation.
- **Unverified on hardware.** The vertex Y convention, the depth-buffer-as-texture read and the linear-aligned surfaces follow GX2's documented behaviour but haven't been run on a console yet. If the 3D appears upside down, the fix is the `-position.y` in `platform/shaders/raster.vert` and `fullscreen.vert` (both, so depth transfers stay aligned).
