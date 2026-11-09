//! what the host GPU's rasterizers share, the Vulkan one (hardware.rs) and
//! the Wii U's GX2 one (gx2.rs): the draw as raster.rs hands it to them,
//! a display transfer, and the words of raster.frag's uniform blocks, which
//! both fill the same way.

use super::{BoundTexture, DepthMap, Screen, TEXTURE_UNIT_BASES};
use crate::format::ColorFormat;
use crate::fog;
use crate::lighting::{Lighting, Tables};
use crate::proctex;
use crate::registers::*;

/// where each combiner stage's registers start.
pub(super) const STAGE_REGISTERS: [usize; 6] = [0x0C0, 0x0C8, 0x0D0, 0x0D8, 0x0F0, 0x0F8];
pub(super) const REG_UPDATE_BUFFER: usize = 0x0E0;
pub(super) const REG_BUFFER_COLOR: usize = 0x0FD;
pub(super) const REG_BLEND_COLOR: usize = 0x103;

/// the procedural texture's registers, at the end of the uniforms.
pub(super) const PROCTEX_WORDS: usize = 8;
/// the fog's color, and room.
pub(super) const FOG_WORDS: usize = 4;
/// the words of a draw's uniform block, the combiners, the texture units
/// and the flags, then the lighting, the procedural texture and the fog.
pub(super) const UNIFORM_WORDS: usize = 12 * 4 + 4 + 3 * 4 + 4 + 328 + PROCTEX_WORDS + FOG_WORDS;
/// the lighting tables, then the procedural texture's, its noise, color
/// map and alpha map as values and steps and its colors and their steps as
/// two pairs each, then the fog's, a value and a step an entry.
pub(super) const TABLES_BYTES: usize = (24 * 256 + 3 * proctex::MAP_ENTRIES + 4 * proctex::COLOR_ENTRIES + fog::ENTRIES) * 8;

/// what a draw needs from the rasterizer.
pub(super) struct Draw<'a> {
    pub(super) registers: &'a [u32],
    pub(super) target: u32,
    pub(super) format: ColorFormat,
    pub(super) width: u32,
    pub(super) height: u32,
    /// the pixels the draw may touch, left, bottom, right and top in window
    /// coordinates, y up.
    pub(super) scissor: [i32; 4],
    /// the depth and stencil buffer and its bytes per sample, when the draw
    /// uses one.
    pub(super) depth: Option<(u32, u32)>,
    pub(super) depth_map: DepthMap,
    pub(super) geometry: Geometry<'a>,
    pub(super) textures: &'a [Option<BoundTexture>; 3],
    pub(super) lighting: Option<&'a Lighting>,
    pub(super) tables: &'a Tables,
    /// whether the procedural texture is on, and its tables.
    pub(super) proctex: bool,
    pub(super) proctex_tables: &'a proctex::Tables,
    /// whether the fog is on, and its table.
    pub(super) fog: bool,
    pub(super) fog_table: &'a fog::Table,
}

/// the triangles of a draw.
#[derive(Clone, Copy)]
pub(super) enum Geometry<'a> {
    /// shaded, clipped and culled on the CPU, placed vertices and the
    /// triangles, three indices each, that share them.
    Placed { vertices: &'a [Screen], indices: &'a [u32] },
    /// for the GPU to shade.
    #[cfg(feature = "vulkan")]
    Shaded(&'a super::hardware::Shading<'a>),
}

/// a display transfer, as the GPU takes it. GX2 leaves them to the CPU.
#[cfg_attr(not(feature = "vulkan"), allow(dead_code))]
pub(crate) struct Transfer {
    pub(crate) input: u32,
    pub(crate) output: u32,
    pub(crate) input_width: u32,
    pub(crate) output_width: u32,
    pub(crate) output_height: u32,
    /// the rectangle copied, in output pixels.
    pub(crate) copy: (u32, u32),
    /// input pixels per output pixel each way, a downscale averaging them.
    pub(crate) scale: (u32, u32),
    pub(crate) flip: bool,
    pub(crate) input_linear: bool,
    pub(crate) output_tiled: bool,
    pub(crate) input_format: ColorFormat,
    pub(crate) output_format: ColorFormat,
}

/// the words of raster.frag's Draw block for a draw, in its std140 order:
/// the combiner stages, the buffer update, buffer color, alpha test and
/// texture configuration, the texture units, the flags, the lighting, the
/// procedural texture and the fog. enabled is the texture units bound, one
/// bit each, shaded whether the GPU shades the vertices. gives the texture
/// configuration and the depth flags it put in.
pub(super) fn pack_uniforms(
    r: &[u32],
    enabled: u32,
    depth_map: DepthMap,
    shaded: bool,
    lighting: Option<&Lighting>,
    words: &mut Vec<u32>,
) -> (u32, u32) {
    words.clear();
    for base in STAGE_REGISTERS {
        words.extend([r[base], r[base + 1], r[base + 2], r[base + 3], r[base + 4], 0, 0, 0]);
    }
    let texture_config = (r[REG_TEXTURE_CONFIG] & !7) | enabled;
    words.extend([r[REG_UPDATE_BUFFER], r[REG_BUFFER_COLOR], r[REG_ALPHA_TEST], texture_config]);
    for base in TEXTURE_UNIT_BASES {
        words.extend([r[base + 2], r[base], 0, 0]);
    }
    let depth_flags = depth_map.w_buffer as u32 | (shaded as u32) << 1;
    words.extend([depth_flags, lighting.is_some() as u32, depth_map.scale.to_bits(), depth_map.offset.to_bits()]);
    match lighting {
        Some(lighting) => lighting.pack(words),
        None => words.resize(UNIFORM_WORDS - PROCTEX_WORDS - FOG_WORDS, 0),
    }
    let procedural = [
        proctex::REG_CONFIG,
        proctex::REG_CONFIG + 1,
        proctex::REG_CONFIG + 2,
        proctex::REG_CONFIG + 3,
        proctex::REG_CONFIG + 4,
        proctex::REG_CONFIG + 5,
    ];
    words.extend(procedural.map(|register| r[register]));
    words.extend([0, 0]);
    words.extend([r[fog::REG_COLOR], 0, 0, 0]);
    debug_assert_eq!(words.len(), UNIFORM_WORDS);
    (texture_config, depth_flags)
}

/// the Tables block, TABLES_BYTES of little-endian floats, four bytes each:
/// the lighting tables, the procedural texture's maps, its colors and their
/// steps, then the fog's table, the order raster.frag reads them in.
pub(super) fn pack_tables(tables: &Tables, procedural: &proctex::Tables, fog: &fog::Table, out: &mut [[u8; 4]]) {
    debug_assert_eq!(out.len() * 4, TABLES_BYTES);
    let (colors, steps) = procedural.colors();
    // a plain loop over each, which goes as fast as copying, a chain of
    // flattened iterators took a quarter of a frame
    let parts = [
        tables.entries().as_flattened().as_flattened(),
        procedural.maps().as_flattened().as_flattened(),
        colors.as_flattened(),
        steps.as_flattened(),
        fog.entries().as_flattened(),
    ];
    let mut out = out;
    for part in parts {
        let (here, rest) = out.split_at_mut(part.len());
        for (bytes, value) in here.iter_mut().zip(part) {
            *bytes = value.to_le_bytes();
        }
        out = rest;
    }
}
