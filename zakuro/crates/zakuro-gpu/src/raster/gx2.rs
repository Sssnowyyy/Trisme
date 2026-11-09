//! filling triangles on the Wii U's GPU through GX2. the CPU shades, clips
//! and places the vertices as the software path does, the GPU does what
//! costs, the pixels, texturing, lighting, the combiners, the tests and
//! blending, running raster.frag built ahead of time with DYNAMIC set, so
//! that it reads the combiners from the Draw block as the Vulkan backend's
//! generic pipelines do.
//!
//! the GPU belongs to the main thread, see platform/zw_gx2.h: this side
//! fills surfaces and textures both see, records commands and submits them,
//! and a submit waits until they ran. outside a submit the GPU leaves what
//! this side holds alone, so there is nothing to overlap or fence, only the
//! order to keep: pixels the CPU writes into a surface reach the GPU before
//! every command of the batch they are submitted with, so a surface the
//! batch already draws into is submitted first.
//!
//! guest memory stays where pictures live between batches, as with the
//! Vulkan backend: a buffer a draw needs goes up the first time memory may
//! have changed under it, and comes back down only when something else is
//! about to read it, a texture, a fill, a display transfer or the CPU.

use std::collections::HashMap;
use std::sync::Arc;

use super::hw::{self, Draw, Geometry, Transfer, REG_BLEND_COLOR, TABLES_BYTES, UNIFORM_WORDS};
use super::{BoundTexture, DepthMap, DrawnTexture, Pass, Screen, Wrap};
use crate::blend::LogicOp;
use crate::format::{morton_offset, ColorFormat};
use crate::registers::*;
use crate::{GpuMemory, Picture, ScreenRef};

/// a vertex, six vectors, position, color, texture coordinates 0 and 1,
/// coordinate 2 with the mapped depth, the quaternion and the view vector,
/// ZW_GX2_VERTEX_SIZE.
const VERTEX_SIZE: usize = 24 * 4;
const UNIFORM_BYTES: usize = UNIFORM_WORDS * 4;
/// ZW_GX2_MAX_LEVELS, the most sizes a texture has.
const MAX_LEVELS: usize = 8;
/// where each part of a batch starts, GX2's uniform blocks want 256 bytes,
/// its vertices 64.
const ALIGN: usize = 256;

const SHADER_LIGHTING: u32 = 1 << 0;
const SHADER_PROCTEX: u32 = 1 << 1;
const SHADER_WRITES_DEPTH: u32 = 1 << 2;

const CMD_DRAW: u32 = 1;
const CMD_CLEAR_COLOR: u32 = 2;
const CMD_CLEAR_DEPTH: u32 = 3;
const CMD_LOAD_DEPTH: u32 = 4;
const CMD_STORE_DEPTH: u32 = 5;

/// textures nobody drew with for this many batches go, and the oldest
/// beyond this many bytes.
const TEXTURE_IDLE: u64 = 600;
const TEXTURE_BUDGET: usize = 96 << 20;
/// past this many surfaces, those nobody drew into for SURFACE_IDLE
/// batches go, written back first where the GPU drew in them.
const MOST_SURFACES: usize = 24;
const SURFACE_IDLE: u64 = 120;
/// the largest level of detail bias a sampler takes either way.
const MOST_BIAS: f32 = 15.0;

/// platform/zw_gx2.h's ZwGx2Level.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Level {
    pixels: *mut u8,
    /// pixels from one row to the next.
    pitch: u32,
    width: u32,
    height: u32,
}

impl Level {
    // SAFETY: a null pointer and zeros
    const NONE: Level = unsafe { std::mem::zeroed() };

    /// rows of the level's RGBA8 pixels, pitch pixels each.
    ///
    /// # Safety
    /// the level is one the platform layer gave, at least rows high, and
    /// the GPU is not using it: no submit is running.
    unsafe fn rows<'a>(&self, rows: u32) -> &'a mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pixels, (self.pitch * rows * 4) as usize) }
    }
}

/// platform/zw_gx2.h's ZwGx2Sampler.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sampler {
    mag_linear: u8,
    min_linear: u8,
    /// 0 one level only, 1 the nearest level, 2 between levels.
    mip: u8,
    /// 0 clamp to edge, 1 to the border, which raster.frag draws, 2 repeat,
    /// 3 mirrored repeat.
    wrap_s: u8,
    wrap_t: u8,
    pad_: [u8; 3],
    min_lod: f32,
    max_lod: f32,
    lod_bias: f32,
}

/// platform/zw_gx2.h's ZwGx2Command.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Command {
    kind: u32,
    color: u32,
    depth: u32,
    /// left, top, right and bottom in target pixels, the image's rows,
    /// right and bottom left out.
    scissor: [i32; 4],
    vertices: *const u8,
    vertex_count: u32,
    indices: *const u32,
    index_count: u32,
    uniforms: *const u8,
    uniforms_size: u32,
    tables: *const u8,
    textures: [u32; 3],
    samplers: [Sampler; 3],
    shader: u32,
    color_mask: u8,
    depth_test: u8,
    depth_func: u8,
    depth_write: u8,
    stencil_test: u8,
    stencil_func: u8,
    stencil_ref: u8,
    stencil_mask: u8,
    stencil_write_mask: u8,
    stencil_fail: u8,
    stencil_zfail: u8,
    stencil_zpass: u8,
    blend_enable: u8,
    blend_eq_rgb: u8,
    blend_eq_alpha: u8,
    blend_src_rgb: u8,
    blend_dst_rgb: u8,
    blend_src_alpha: u8,
    blend_dst_alpha: u8,
    logic_op_enable: u8,
    logic_op: u8,
    pad_: [u8; 2],
    blend_color: u32,
    clear_color: [f32; 4],
    clear_depth: f32,
    clear_stencil: u8,
    clear_flags: u8,
    pad2_: [u8; 2],
}

impl Command {
    // SAFETY: integers, floats and null pointers
    const NONE: Command = unsafe { std::mem::zeroed() };
}

// the layouts powerpc-eabi-gcc -meabi gives platform/zw_gx2.h's structs
#[cfg(target_vendor = "wiiu")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Level>() == 16 && size_of::<Sampler>() == 20);
    assert!(offset_of!(Command, scissor) == 12 && offset_of!(Command, vertices) == 28 && offset_of!(Command, tables) == 52);
    assert!(offset_of!(Command, samplers) == 68 && offset_of!(Command, shader) == 128 && offset_of!(Command, color_mask) == 132);
    assert!(offset_of!(Command, blend_color) == 156 && offset_of!(Command, clear_color) == 160 && offset_of!(Command, clear_flags) == 181);
    assert!(size_of::<Command>() == 184);
};

/// the platform layer's side of platform/zw_gx2.h, linked into the program.
#[cfg(target_vendor = "wiiu")]
mod ffi {
    use super::{Command, Level};

    extern "C" {
        pub(super) fn zw_gx2_available() -> i32;
        pub(super) fn zw_gx2_color_create(width: u32, height: u32, level: *mut Level) -> u32;
        pub(super) fn zw_gx2_depth_create(width: u32, height: u32) -> u32;
        pub(super) fn zw_gx2_texture_create(width: u32, height: u32, levels: u32, level_out: *mut Level) -> u32;
        pub(super) fn zw_gx2_written(handle: u32);
        pub(super) fn zw_gx2_destroy(handle: u32);
        pub(super) fn zw_gx2_alloc(size: u32, align: u32) -> *mut std::ffi::c_void;
        pub(super) fn zw_gx2_submit(commands: *const Command, count: u32, readback: *const u32, readback_count: u32) -> i32;
    }
}

/// off the Wii U there is no GX2. these stand in for the platform layer's
/// calls: never available outside the tests, and in them a CPU doing what
/// the tests look at, the clears and the depth's trips through color, the
/// draws only kept, not filled.
#[cfg(not(target_vendor = "wiiu"))]
mod ffi {
    use super::{Command, Level, CMD_CLEAR_COLOR, CMD_CLEAR_DEPTH, CMD_DRAW, CMD_LOAD_DEPTH, CMD_STORE_DEPTH, VERTEX_SIZE};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// a surface or texture, its levels' pixels and pitches, or a depth
    /// buffer's samples, the depth in the low 24 bits and the stencil above.
    struct Object {
        levels: Vec<(Box<[u8]>, Level)>,
        width: u32,
        depth: Vec<u32>,
    }

    #[derive(Default)]
    pub(super) struct Fake {
        pub(super) available: bool,
        objects: HashMap<u32, Object>,
        next: u32,
        blocks: Vec<Box<[u8]>>,
        /// the bytes a batch may take, to run out of.
        pub(super) budget: Option<usize>,
        used: usize,
        /// every command submitted, and each draw's vertices as bytes.
        pub(super) submitted: Vec<Command>,
        pub(super) vertices: Vec<Vec<u8>>,
        pub(super) submits: usize,
    }

    thread_local! {
        static FAKE: RefCell<Fake> = RefCell::new(Fake::default());
    }

    pub(super) fn with<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
        FAKE.with(|fake| f(&mut fake.borrow_mut()))
    }

    impl Fake {
        fn add(&mut self, object: Object) -> u32 {
            self.next += 1;
            self.objects.insert(self.next, object);
            self.next
        }

        /// level 0 of a surface or texture, rows packed.
        #[cfg(test)]
        pub(super) fn pixels(&self, handle: u32) -> Vec<u8> {
            let (bytes, level) = &self.objects[&handle].levels[0];
            let row = level.width as usize * 4;
            (0..level.height as usize).flat_map(|y| bytes[y * level.pitch as usize * 4..][..row].iter().copied()).collect()
        }

        /// a pixel of level 0 as the GPU drew it.
        #[cfg(test)]
        pub(super) fn set_pixel(&mut self, handle: u32, x: u32, y: u32, rgba: [u8; 4]) {
            let (bytes, level) = self.objects.get_mut(&handle).unwrap().levels.get_mut(0).unwrap();
            let at = ((y * level.pitch + x) * 4) as usize;
            bytes[at..at + 4].copy_from_slice(&rgba);
        }

        #[cfg(test)]
        pub(super) fn depth(&self, handle: u32) -> Vec<u32> {
            self.objects[&handle].depth.clone()
        }

        #[cfg(test)]
        pub(super) fn set_depth(&mut self, handle: u32, x: u32, y: u32, value: u32) {
            let object = self.objects.get_mut(&handle).unwrap();
            let width = object.width;
            object.depth[(y * width + x) as usize] = value;
        }

        #[cfg(test)]
        pub(super) fn live(&self) -> usize {
            self.objects.len()
        }

        fn run(&mut self, command: &Command) {
            let pixels_of = |object: &mut Object, f: &mut dyn FnMut(u32, u32, &mut [u8])| {
                let (bytes, level) = &mut object.levels[0];
                for y in 0..level.height {
                    for x in 0..level.width {
                        let at = ((y * level.pitch + x) * 4) as usize;
                        f(x, y, &mut bytes[at..at + 4]);
                    }
                }
            };
            match command.kind {
                CMD_CLEAR_COLOR => {
                    let color = command.clear_color.map(|c| (c * 255.0).round() as u8);
                    pixels_of(self.objects.get_mut(&command.color).unwrap(), &mut |_, _, pixel| pixel.copy_from_slice(&color));
                }
                CMD_CLEAR_DEPTH => {
                    let d24 = (command.clear_depth as f64 * 16_777_215.0).round() as u32;
                    for sample in &mut self.objects.get_mut(&command.depth).unwrap().depth {
                        if command.clear_flags & 1 != 0 {
                            *sample = *sample & 0xFF00_0000 | d24;
                        }
                        if command.clear_flags & 2 != 0 {
                            *sample = *sample & 0xFF_FFFF | (command.clear_stencil as u32) << 24;
                        }
                    }
                }
                CMD_LOAD_DEPTH => {
                    let mut values = Vec::new();
                    pixels_of(self.objects.get_mut(&command.textures[0]).unwrap(), &mut |_, _, p| {
                        values.push(p[0] as u32 | (p[1] as u32) << 8 | (p[2] as u32) << 16)
                    });
                    for (sample, value) in self.objects.get_mut(&command.depth).unwrap().depth.iter_mut().zip(values) {
                        *sample = *sample & 0xFF00_0000 | value;
                    }
                }
                CMD_STORE_DEPTH => {
                    let depth = self.objects[&command.depth].depth.clone();
                    let width = self.objects[&command.depth].width;
                    pixels_of(self.objects.get_mut(&command.color).unwrap(), &mut |x, y, pixel| {
                        let [a, b, c, _] = depth[(y * width + x) as usize].to_le_bytes();
                        pixel.copy_from_slice(&[a, b, c, 0]);
                    });
                }
                CMD_DRAW => {
                    // SAFETY: the vertices are in a block of this batch
                    let bytes = unsafe { std::slice::from_raw_parts(command.vertices, command.vertex_count as usize * VERTEX_SIZE) };
                    self.vertices.push(bytes.to_vec());
                }
                _ => {}
            }
            self.submitted.push(*command);
        }
    }

    fn levels(width: u32, height: u32, count: u32) -> Vec<(Box<[u8]>, Level)> {
        (0..count)
            .map(|level| {
                let (width, height) = ((width >> level).max(1), (height >> level).max(1));
                // rows longer than the picture, as GX2's are
                let pitch = width + 8;
                let mut bytes = vec![0u8; (pitch * height * 4) as usize].into_boxed_slice();
                let level = Level { pixels: bytes.as_mut_ptr(), pitch, width, height };
                (bytes, level)
            })
            .collect()
    }

    pub(super) unsafe fn zw_gx2_available() -> i32 {
        with(|fake| fake.available as i32)
    }

    pub(super) unsafe fn zw_gx2_color_create(width: u32, height: u32, level: *mut Level) -> u32 {
        with(|fake| {
            let levels = levels(width, height, 1);
            unsafe { *level = levels[0].1 };
            fake.add(Object { levels, width, depth: Vec::new() })
        })
    }

    pub(super) unsafe fn zw_gx2_depth_create(width: u32, height: u32) -> u32 {
        with(|fake| fake.add(Object { levels: Vec::new(), width, depth: vec![0; (width * height) as usize] }))
    }

    pub(super) unsafe fn zw_gx2_texture_create(width: u32, height: u32, count: u32, level_out: *mut Level) -> u32 {
        with(|fake| {
            let levels = levels(width, height, count);
            for (i, (_, level)) in levels.iter().enumerate() {
                unsafe { *level_out.add(i) = *level };
            }
            fake.add(Object { levels, width, depth: Vec::new() })
        })
    }

    pub(super) unsafe fn zw_gx2_written(_handle: u32) {}

    pub(super) unsafe fn zw_gx2_destroy(handle: u32) {
        with(|fake| fake.objects.remove(&handle));
    }

    pub(super) unsafe fn zw_gx2_alloc(size: u32, align: u32) -> *mut std::ffi::c_void {
        with(|fake| {
            if fake.budget.is_some_and(|budget| fake.used + size as usize > budget) {
                return std::ptr::null_mut();
            }
            fake.used += size as usize;
            let mut block = vec![0u8; (size + align) as usize].into_boxed_slice();
            let start = block.as_mut_ptr();
            let at = start.align_offset(align as usize);
            fake.blocks.push(block);
            unsafe { start.add(at).cast() }
        })
    }

    pub(super) unsafe fn zw_gx2_submit(commands: *const Command, count: u32, _readback: *const u32, _readback_count: u32) -> i32 {
        with(|fake| {
            for i in 0..count as usize {
                let command = unsafe { *commands.add(i) };
                fake.run(&command);
            }
            fake.blocks.clear();
            fake.used = 0;
            fake.submits += 1;
            0
        })
    }
}

fn destroy(handle: u32) {
    if handle != 0 {
        // SAFETY: a handle this renderer made, which nothing submitted
        // uses any more
        unsafe { ffi::zw_gx2_destroy(handle) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Color(ColorFormat),
    /// bytes per sample, 2 for D16, 3 for D24 and 4 for D24S8.
    Depth(u32),
}

impl Kind {
    fn bytes(self) -> u32 {
        match self {
            Kind::Color(format) => format.bytes_per_pixel() as u32,
            Kind::Depth(bytes) => bytes,
        }
    }
}

/// a color or depth buffer of the guest's, tiled as the PICA draws, kept on
/// the GPU with its rows bottom first, the other way from memory, as the
/// Vulkan backend keeps its images: the image's row 0 is the window's
/// bottom row, memory's last.
struct Surface {
    addr: u32,
    width: u32,
    height: u32,
    kind: Kind,
    /// the GPU's target, an RGBA8 color one or a depth and stencil one.
    target: u32,
    /// a color target's pixels.
    level: Level,
    /// a depth buffer's way up, an RGBA8 texture holding the depth, and its
    /// way down, a color target the depth is copied into, each made the
    /// first time it is needed.
    load: Option<(u32, Level)>,
    store: Option<(u32, Level)>,
    /// the guest's bytes the last time the image matched them.
    shadow: Vec<u8>,
    /// the pixel all of those are, when a fill left them so or memory held
    /// nothing else, standing in for them.
    pixel: Option<[u8; 4]>,
    /// the image is known to match guest memory in this batch.
    checked: bool,
    /// the rows drawn into since guest memory last got the image, rows of
    /// memory from the buffer's start, whole rows of tiles.
    dirty: Option<(u32, u32)>,
    /// the rows of those whose reads, and writes, the CPU was told to ask
    /// for first.
    guarded: Option<(u32, u32)>,
    write_guarded: Option<(u32, u32)>,
    /// a command of the batch uses it, so the CPU leaves its pixels alone
    /// until the batch is submitted.
    pending: bool,
    /// the batch that last drew into it.
    used: u64,
}

impl Surface {
    fn size(&self) -> u32 {
        self.width * self.height * self.kind.bytes()
    }

    fn overlaps(&self, addr: u32, len: u32) -> bool {
        (addr as u64) < self.addr as u64 + self.size() as u64 && (self.addr as u64) < addr as u64 + len as u64
    }

    fn row_bytes(&self) -> u32 {
        self.width * self.kind.bytes()
    }

    /// all of the target, as a scissor.
    fn whole(&self) -> [i32; 4] {
        [0, 0, self.width as i32, self.height as i32]
    }

    /// whether the shadow holds bytes over a range of the buffer's memory,
    /// one starting at a pixel.
    fn shadows(&self, range: std::ops::Range<usize>, bytes: &[u8]) -> bool {
        match self.pixel {
            Some(pixel) => {
                range.end <= self.size() as usize
                    && bytes.len() == range.len()
                    && crate::pattern::holds(bytes, &pixel[..self.kind.bytes() as usize])
            }
            None => self.shadow.get(range) == Some(bytes),
        }
    }

    /// has the shadow hold the buffer's bytes as memory does.
    fn keep(&mut self, bytes: &[u8]) {
        let bpp = self.kind.bytes() as usize;
        let size = self.size() as usize;
        match bytes.get(..bpp).filter(|pixel| bytes.len() == size && crate::pattern::holds(bytes, pixel)) {
            Some(pixel) => self.keep_pixel(pixel),
            None => {
                self.pixel = None;
                self.shadow.clear();
                self.shadow.extend_from_slice(bytes);
            }
        }
    }

    fn keep_pixel(&mut self, pixel: &[u8]) {
        let mut kept = [0; 4];
        kept[..pixel.len()].copy_from_slice(pixel);
        self.pixel = Some(kept);
    }

    /// the shadow's bytes, written out where a pixel stood in for them.
    fn shadow(&mut self) -> &mut Vec<u8> {
        if let Some(pixel) = self.pixel.take() {
            self.shadow.resize(self.size() as usize, 0);
            crate::pattern::fill(&mut self.shadow, &pixel[..self.kind.bytes() as usize]);
        }
        &mut self.shadow
    }

    /// the memory the rows drawn take.
    fn dirty_span(&self) -> Option<(u64, u64)> {
        let row = self.row_bytes() as u64;
        self.dirty.map(|(start, end)| (self.addr as u64 + start as u64 * row, self.addr as u64 + end as u64 * row))
    }

    /// whether guest memory over a range is behind the image, where the GPU
    /// drew.
    fn dirty_overlaps(&self, addr: u32, len: u32) -> bool {
        self.dirty_span().is_some_and(|(from, to)| from < to && (addr as u64) < to && from < addr as u64 + len as u64)
    }

    /// whether all the rows drawn lie in a range.
    fn dirty_within(&self, addr: u32, len: u32) -> bool {
        self.dirty_span().is_some_and(|(from, to)| addr as u64 <= from && to <= addr as u64 + len as u64)
    }

    /// the GPU changed rows of the image, rows of memory from the start.
    fn drew(&mut self, (start, end): (u32, u32)) {
        let rows = ((start / 8 * 8).min(self.height), end.div_ceil(8).saturating_mul(8).min(self.height));
        self.dirty = Some(match self.dirty {
            Some((from, to)) => (from.min(rows.0), to.max(rows.1)),
            None => rows,
        });
    }

    /// has the CPU's reads of the rows drawn ask for them first.
    fn guard<M: GpuMemory>(&mut self, memory: &mut M) {
        let row = self.row_bytes();
        if let Some((addr, len)) = widen(&mut self.guarded, self.dirty, self.addr, row) {
            memory.guard(addr, len);
        }
    }

    /// has the rows drawn reach guest memory before the CPU writes over
    /// them, as on the console, where the GPU put its pixels there at once.
    fn guard_writes<M: GpuMemory>(&mut self, memory: &mut M) {
        let row = self.row_bytes();
        if let Some((addr, len)) = widen(&mut self.write_guarded, self.dirty, self.addr, row) {
            memory.guard_writes(addr, len);
        }
    }

    /// the image is no longer known to be anything memory held.
    fn lose(&mut self) {
        self.shadow.clear();
        self.pixel = None;
        self.dirty = None;
        self.guarded = None;
        self.write_guarded = None;
        self.checked = false;
    }

    fn destroy(&self) {
        destroy(self.target);
        for (handle, _) in [self.load, self.store].into_iter().flatten() {
            destroy(handle);
        }
    }
}

/// the range of memory to guard for the rows drawn, which joins the rows
/// guarded before, none when they all were.
fn widen(guarded: &mut Option<(u32, u32)>, dirty: Option<(u32, u32)>, addr: u32, row: u32) -> Option<(u32, u32)> {
    let (start, end) = dirty?;
    if guarded.is_some_and(|(from, to)| from <= start && end <= to) {
        return None;
    }
    let (start, end) = guarded.map_or((start, end), |(from, to)| (from.min(start), to.max(end)));
    *guarded = Some((start, end));
    Some((addr + start * row, (end - start) * row))
}

/// a texture uploaded, which the decoded texels it was made from key.
struct Texture {
    handle: u32,
    /// holds the texels the key points at, so the key stays theirs.
    _texels: Arc<[[u8; 4]]>,
    bytes: usize,
    used: u64,
}

/// the texels' address, the size and how many sizes.
type TextureKey = (usize, u32, u32, u32);

pub(crate) struct Hardware {
    pub(crate) pass: Pass,
    surfaces: Vec<Surface>,
    textures: HashMap<TextureKey, Texture>,
    texture_bytes: usize,
    /// the batch being recorded.
    commands: Vec<Command>,
    /// counts the batches submitted.
    batch: u64,
    /// the batch trim last looked at.
    trimmed: u64,
    /// the Draw block's words, the draw's and the last staged, and where
    /// the batch holds those.
    words: Vec<u32>,
    last_words: Vec<u32>,
    uniforms_at: Option<*const u8>,
    /// the tables the batch holds, by their generations, and where.
    tables_at: Option<((u64, u64, u64), *const u8)>,
    scratch: Vec<u8>,
    /// making a surface failed for want of memory, the next draw lets go of
    /// the others first.
    starved: bool,
    /// a depth buffer's stencil went up as one value, said once.
    told_stencil: bool,
}

// SAFETY: the pointers are the platform layer's surfaces and the batch's
// memory, which only this renderer's thread uses
unsafe impl Send for Hardware {}

impl Hardware {
    /// the GX2 rasterizer, when the platform layer has one to offer.
    pub fn new() -> Result<Hardware, String> {
        // SAFETY: a plain query of the platform layer
        if unsafe { ffi::zw_gx2_available() } != 1 {
            return Err("the platform layer has no GX2 rasterizer, its shaders are missing or did not load".into());
        }
        Ok(Hardware {
            pass: Pass::Real,
            surfaces: Vec::new(),
            textures: HashMap::new(),
            texture_bytes: 0,
            commands: Vec::new(),
            batch: 0,
            trimmed: 0,
            words: Vec::with_capacity(UNIFORM_WORDS),
            last_words: Vec::with_capacity(UNIFORM_WORDS),
            uniforms_at: None,
            tables_at: None,
            scratch: Vec::new(),
            starved: false,
            told_stencil: false,
        })
    }

    pub fn name(&self) -> &str {
        "the Wii U's GPU through GX2"
    }

    /// whether the GPU runs the vertex shaders, which it leaves to the CPU.
    pub(crate) fn shades(&self) -> bool {
        false
    }

    /// it draws at the console's resolution.
    pub fn scale(&self) -> u32 {
        1
    }

    /// the screens are shown from guest memory, not straight from the GPU.
    pub(crate) fn direct(&self) -> bool {
        false
    }

    /// frame interpolation takes a presenter sharing the GPU, which there
    /// is none of.
    pub(crate) fn twins(&self) -> bool {
        false
    }

    pub(crate) fn set_twins(&mut self, _twins: bool) {}

    /// runs the batch, the surfaces in readback then the CPU's to read.
    fn submit(&mut self, readback: &[u32]) -> Result<(), String> {
        if self.commands.is_empty() && readback.is_empty() {
            return Ok(());
        }
        // SAFETY: the commands point at memory zw_gx2_alloc gave this batch
        // and at handles this renderer made, all there until it returns
        let result = unsafe {
            ffi::zw_gx2_submit(self.commands.as_ptr(), self.commands.len() as u32, readback.as_ptr(), readback.len() as u32)
        };
        self.commands.clear();
        self.batch += 1;
        self.uniforms_at = None;
        self.tables_at = None;
        for surface in &mut self.surfaces {
            surface.pending = false;
            surface.checked = false;
        }
        match result {
            0.. => Ok(()),
            _ => Err("the GPU dropped a batch, the program is closing".into()),
        }
    }

    /// lets go of surfaces and textures no longer drawn with, or of all of
    /// them after making one failed, between draws, when no draw holds
    /// their handles.
    fn trim<M: GpuMemory>(&mut self, memory: &mut M) -> Result<(), String> {
        if !self.starved && self.trimmed == self.batch {
            return Ok(());
        }
        let starved = std::mem::take(&mut self.starved);
        let batch = self.batch;
        if starved || self.surfaces.len() > MOST_SURFACES {
            let gone: Vec<bool> =
                self.surfaces.iter().map(|s| !s.pending && (starved || batch.saturating_sub(s.used) > SURFACE_IDLE)).collect();
            let dirty: Vec<usize> = (0..self.surfaces.len()).filter(|&i| gone[i] && self.surfaces[i].dirty.is_some()).collect();
            self.write_back(memory, dirty)?;
            let mut gone = gone.into_iter();
            self.surfaces.retain(|surface| {
                let gone = gone.next().unwrap_or(false);
                if gone {
                    surface.destroy();
                }
                !gone
            });
        }
        // what the batch draws with stays until it is submitted
        let batch = self.batch;
        let idle: Vec<TextureKey> = self
            .textures
            .iter()
            .filter(|(_, t)| t.used < batch && (starved || batch - t.used > TEXTURE_IDLE))
            .map(|(&key, _)| key)
            .collect();
        for key in idle {
            self.drop_texture(key);
        }
        while self.texture_bytes > TEXTURE_BUDGET {
            let oldest = self.textures.iter().filter(|(_, t)| t.used < batch).min_by_key(|(_, t)| t.used).map(|(&key, _)| key);
            match oldest {
                Some(key) => self.drop_texture(key),
                None => break,
            }
        }
        self.trimmed = self.batch;
        Ok(())
    }

    fn drop_texture(&mut self, key: TextureKey) {
        if let Some(texture) = self.textures.remove(&key) {
            destroy(texture.handle);
            self.texture_bytes -= texture.bytes;
        }
    }

    /// a target for a surface, and a color one's pixels.
    fn create(&mut self, kind: Kind, width: u32, height: u32) -> Result<(u32, Level), String> {
        let mut level = Level::NONE;
        // SAFETY: plain calls of the platform layer, level has room for one
        let target = unsafe {
            match kind {
                Kind::Color(_) => ffi::zw_gx2_color_create(width, height, &mut level),
                Kind::Depth(_) => ffi::zw_gx2_depth_create(width, height),
            }
        };
        if target == 0 {
            self.starved = true;
            return Err(format!("no GPU memory left for a {width}x{height} buffer"));
        }
        if matches!(kind, Kind::Color(_)) && (level.pixels.is_null() || level.pitch < width || level.height < height) {
            destroy(target);
            return Err(format!("the platform layer made a {width}x{height} buffer too small"));
        }
        Ok((target, level))
    }

    /// a depth surface's texture its depth goes up through.
    fn depth_load(&mut self, index: usize) -> Result<(u32, Level), String> {
        if let Some(load) = self.surfaces[index].load {
            return Ok(load);
        }
        let (width, height) = (self.surfaces[index].width, self.surfaces[index].height);
        let mut level = Level::NONE;
        // SAFETY: a plain call of the platform layer, level has room for one
        let texture = unsafe { ffi::zw_gx2_texture_create(width, height, 1, &mut level) };
        if texture == 0 || level.pixels.is_null() || level.pitch < width || level.height < height {
            destroy(texture);
            self.starved = true;
            return Err(format!("no GPU memory left to load a {width}x{height} depth buffer"));
        }
        self.surfaces[index].load = Some((texture, level));
        Ok((texture, level))
    }

    /// a depth surface's color target its depth comes down through.
    fn depth_store(&mut self, index: usize) -> Result<(u32, Level), String> {
        if let Some(store) = self.surfaces[index].store {
            return Ok(store);
        }
        let (width, height) = (self.surfaces[index].width, self.surfaces[index].height);
        let store = self.create(Kind::Color(ColorFormat::Rgba8), width, height)?;
        self.surfaces[index].store = Some(store);
        Ok(store)
    }

    /// the height of the tallest surface of a kind at an address and width,
    /// at least height, the one a draw of that height goes into.
    fn taller(&self, addr: u32, kind: Kind, width: u32, height: u32) -> u32 {
        self.surfaces.iter().filter(|s| s.addr == addr && s.kind == kind && s.width == width).map(|s| s.height).fold(height, u32::max)
    }

    /// the surface for a guest buffer, matching guest memory over the rows
    /// of memory given, or all of them.
    fn surface_rows<M: GpuMemory>(
        &mut self,
        memory: &mut M,
        addr: u32,
        (width, height): (u32, u32),
        kind: Kind,
        rows: Option<(u32, u32)>,
    ) -> Result<usize, String> {
        let found = self.surfaces.iter().position(|s| s.addr == addr && s.width == width && s.height == height && s.kind == kind);
        let index = match found {
            Some(index) => index,
            None => {
                let (target, level) = self.create(kind, width, height)?;
                self.surfaces.push(Surface {
                    addr,
                    width,
                    height,
                    kind,
                    target,
                    level,
                    load: None,
                    store: None,
                    shadow: Vec::new(),
                    pixel: None,
                    checked: false,
                    dirty: None,
                    guarded: None,
                    write_guarded: None,
                    pending: false,
                    used: self.batch,
                });
                self.surfaces.len() - 1
            }
        };
        self.surfaces[index].used = self.batch;
        if self.surfaces[index].checked {
            return Ok(index);
        }
        // what another surface over the same memory drew has to reach guest
        // memory before this one reads it
        let size = self.surfaces[index].size();
        let row = self.surfaces[index].row_bytes();
        let (from, to) = rows.map_or((addr, size), |(start, end)| (addr + start * row, (end - start) * row));
        let others: Vec<usize> =
            (0..self.surfaces.len()).filter(|&i| i != index && self.surfaces[i].dirty_overlaps(from, to)).collect();
        self.write_back(memory, others)?;
        let mut bytes = std::mem::take(&mut self.scratch);
        let whole = 0..size as usize;
        let same = memory.slice(addr, size as usize).is_some_and(|now| self.surfaces[index].shadows(whole.clone(), now));
        if !same {
            bytes.resize(size as usize, 0);
            memory.read(addr, &mut bytes);
        }
        if !same && !self.surfaces[index].shadows(whole, &bytes) {
            if self.surfaces[index].dirty.is_some() {
                // memory changed beside rows the GPU drew and it lacks, which
                // an upload alone would lose. they come down first
                self.write_back(memory, vec![index])?;
                memory.read(addr, &mut bytes);
            }
            self.upload(index, &bytes)?;
            self.surfaces[index].keep(&bytes);
        }
        self.scratch = bytes;
        self.surfaces[index].checked = true;
        Ok(index)
    }

    /// puts guest memory's bytes of a buffer into its surface, as a clear
    /// where they are all one pixel.
    fn upload(&mut self, index: usize, bytes: &[u8]) -> Result<(), String> {
        let (kind, width, height, target) = {
            let s = &self.surfaces[index];
            (s.kind, s.width, s.height, s.target)
        };
        let bpp = kind.bytes() as usize;
        let whole = self.surfaces[index].whole();
        if bytes.len() >= bpp && crate::pattern::holds(bytes, &bytes[..bpp]) {
            self.commands.push(clear(kind, target, &bytes[..bpp], whole));
            self.surfaces[index].pending = true;
            return Ok(());
        }
        match kind {
            Kind::Color(format) => {
                // the batch's commands drawing into it go first
                if self.surfaces[index].pending {
                    self.submit(&[])?;
                }
                let level = self.surfaces[index].level;
                // SAFETY: the surface's own level, which no submit is using
                let pixels = unsafe { level.rows(height) };
                color_up(format, bytes, width, height, pixels, level.pitch);
                // SAFETY: a handle this renderer made
                unsafe { ffi::zw_gx2_written(target) };
            }
            Kind::Depth(sample) => {
                let (texture, level) = self.depth_load(index)?;
                if self.surfaces[index].pending {
                    self.submit(&[])?;
                }
                // SAFETY: the surface's own texture, which no submit is using
                let pixels = unsafe { level.rows(height) };
                let stencil = depth_up(sample, bytes, width, height, pixels, level.pitch);
                // SAFETY: a handle this renderer made
                unsafe { ffi::zw_gx2_written(texture) };
                if sample == 4 {
                    // the GPU can't write the stencil from a texture, it is
                    // cleared to the first pixel's
                    let (value, same) = stencil;
                    if !same && !self.told_stencil {
                        log::warn!("a depth buffer's stencil went up to the GPU as one value, GX2 can't load it from memory");
                        self.told_stencil = true;
                    }
                    let mut command = clear(kind, target, &[0, 0, 0, value], whole);
                    command.clear_flags = 2;
                    self.commands.push(command);
                }
                let mut command = Command { kind: CMD_LOAD_DEPTH, depth: target, scissor: whole, ..Command::NONE };
                command.textures[0] = texture;
                self.commands.push(command);
                self.surfaces[index].pending = true;
            }
        }
        Ok(())
    }

    /// the GPU drew rows of a surface, which other surfaces over the same
    /// memory hold older, the next use of one looks at memory again.
    fn overdrawn(&mut self, index: usize, (start, end): (u32, u32), pair: Option<usize>) {
        let s = &self.surfaces[index];
        let row = s.row_bytes();
        let (start, end) = (start / 8 * 8, end.div_ceil(8).saturating_mul(8).min(s.height));
        let (addr, len) = (s.addr + start * row, end.saturating_sub(start) * row);
        for i in 0..self.surfaces.len() {
            if i != index && Some(i) != pair && self.surfaces[i].overlaps(addr, len) {
                self.surfaces[i].checked = false;
            }
        }
    }

    /// a texture's handle, uploaded the first time it is drawn with, all
    /// its sizes. keep are the handles the draw has already, which stay
    /// when room has to be made.
    fn texture(&mut self, bound: &BoundTexture, keep: &[u32]) -> Result<u32, String> {
        let key = (Arc::as_ptr(&bound.texels) as *const u8 as usize, bound.width, bound.height, bound.levels);
        if let Some(texture) = self.textures.get_mut(&key) {
            texture.used = self.batch;
            return Ok(texture.handle);
        }
        let levels = (bound.levels as usize).clamp(1, MAX_LEVELS);
        let sizes: Vec<(u32, u32)> = (0..levels).map(|l| ((bound.width >> l).max(1), (bound.height >> l).max(1))).collect();
        let texels: usize = sizes.iter().map(|&(w, h)| (w * h) as usize).sum();
        if bound.width == 0 || bound.height == 0 || bound.texels.len() < texels {
            return Err("a texture's texels are missing".into());
        }
        let mut out = [Level::NONE; MAX_LEVELS];
        let create = |out: &mut [Level; MAX_LEVELS]| {
            // SAFETY: out has room for MAX_LEVELS levels
            unsafe { ffi::zw_gx2_texture_create(bound.width, bound.height, levels as u32, out.as_mut_ptr()) }
        };
        let mut handle = create(&mut out);
        if handle == 0 {
            // what the batch draws with goes, once it has drawn, all but
            // what this draw holds
            self.submit(&[])?;
            let others: Vec<TextureKey> = self.textures.iter().filter(|(_, t)| !keep.contains(&t.handle)).map(|(&k, _)| k).collect();
            for key in others {
                self.drop_texture(key);
            }
            handle = create(&mut out);
            if handle == 0 {
                return Err(format!("no GPU memory left for a {}x{} texture", bound.width, bound.height));
            }
        }
        let texels = bound.texels.as_flattened();
        let mut at = 0;
        for (level, &(width, height)) in out.iter().zip(&sizes) {
            if level.pixels.is_null() {
                destroy(handle);
                return Err("the platform layer made a texture without its levels".into());
            }
            let (copied, rows) = (width.min(level.width) as usize * 4, height.min(level.height));
            // SAFETY: the texture's own level, which nothing has used yet
            let pixels = unsafe { level.rows(rows) };
            for y in 0..rows as usize {
                let from = &texels[at + y * width as usize * 4..][..copied];
                pixels[y * level.pitch as usize * 4..][..copied].copy_from_slice(from);
            }
            at += (width * height) as usize * 4;
        }
        // SAFETY: a handle this renderer made
        unsafe { ffi::zw_gx2_written(handle) };
        let bytes = texels.len().min(at);
        self.texture_bytes += bytes;
        self.textures.insert(key, Texture { handle, _texels: bound.texels.clone(), bytes, used: self.batch });
        Ok(handle)
    }

    pub(super) fn draw<M: GpuMemory>(&mut self, memory: &mut M, draw: &Draw) -> Result<(), String> {
        if self.pass == Pass::Twin {
            return Ok(());
        }
        let (vertices, indices) = match draw.geometry {
            Geometry::Placed { vertices, indices } => (vertices, indices),
            #[cfg(feature = "vulkan")]
            Geometry::Shaded(_) => return Err("GX2 draws only vertices the CPU placed".into()),
        };
        let [left, bottom, right, top] = draw.scissor;
        let (left, bottom) = (left.max(0), bottom.max(0));
        let (right, top) = (right.min(draw.width as i32), top.min(draw.height as i32));
        if indices.is_empty() || right <= left || top <= bottom {
            return Ok(());
        }
        self.trim(memory)?;

        // the rows of memory the draw can touch, window rows run up from the
        // bottom of the buffer
        let rows = ((draw.height as i32 - top) as u32, (draw.height as i32 - bottom) as u32);
        // titles draw both screens into one buffer, the bottom one into the
        // start of the top one's memory. a taller buffer over the same memory
        // takes the draw, its first rows of memory are the window's top rows,
        // so the window goes up by the rows it lacks
        let height = self.taller(draw.target, Kind::Color(draw.format), draw.width, draw.height);
        let height = match draw.depth {
            Some((addr, bytes)) if self.taller(addr, Kind::Depth(bytes), draw.width, draw.height) != height => draw.height,
            _ => height,
        };
        let raise = (height - draw.height) as i32;
        // a write back looking one of them up drops what the other had checked
        let (color, depth) = loop {
            let size = (draw.width, height);
            let color = self.surface_rows(memory, draw.target, size, Kind::Color(draw.format), Some(rows))?;
            let depth = match draw.depth {
                Some((addr, bytes)) => Some(self.surface_rows(memory, addr, size, Kind::Depth(bytes), Some(rows))?),
                None => None,
            };
            if self.surfaces[color].checked && depth.is_none_or(|d| self.surfaces[d].checked) {
                break (color, depth);
            }
        };
        let (bottom, top) = (bottom + raise, top + raise);

        let mut command = Command { kind: CMD_DRAW, color: self.surfaces[color].target, ..Command::NONE };
        command.depth = depth.map_or(0, |d| self.surfaces[d].target);
        command.scissor = [left, bottom, right, top];
        let mut enabled = 0;
        for (unit, bound) in draw.textures.iter().enumerate() {
            let Some(bound) = bound else { continue };
            command.textures[unit] = self.texture(bound, &command.textures)?;
            command.samplers[unit] = sampler(bound);
            enabled |= 1 << unit;
        }

        let r = draw.registers;
        let depth_map = draw.depth_map;
        let (texture_config, _) = hw::pack_uniforms(r, enabled, depth_map, false, draw.lighting, &mut self.words);
        let tables_read = draw.lighting.is_some() || draw.proctex || draw.fog;
        command.shader = if draw.lighting.is_some() { SHADER_LIGHTING } else { 0 }
            | if texture_config & 0x400 != 0 { SHADER_PROCTEX } else { 0 }
            // what the CPU placed carries its depth in its position, but a
            // w-buffer is not linear on the screen, the shader works it out
            | if depth_map.w_buffer { SHADER_WRITES_DEPTH } else { 0 };

        // the vertices, the indices, and the blocks the draw before did not
        // stage already, in one piece of the batch, which a batch out of
        // room is submitted to make
        let generation = (draw.tables.generation(), draw.proctex_tables.generation(), draw.fog_table.generation());
        let (vertex_bytes, index_bytes) = (vertices.len() * VERTEX_SIZE, indices.len() * 4);
        let mut retried = false;
        loop {
            let uniforms_kept = self.uniforms_at.filter(|_| self.words == self.last_words);
            let tables_kept = self.tables_at.filter(|&(staged, _)| staged == generation).map(|(_, at)| at);
            let mut size = 0;
            let mut place = |bytes: usize| {
                let at = size;
                size += bytes.next_multiple_of(ALIGN);
                at
            };
            let vertex_at = place(vertex_bytes);
            let index_at = place(index_bytes);
            let uniform_at = uniforms_kept.is_none().then(|| place(UNIFORM_BYTES));
            let tables_at = (tables_read && tables_kept.is_none()).then(|| place(TABLES_BYTES));
            // SAFETY: a plain call of the platform layer
            let block = unsafe { ffi::zw_gx2_alloc(size as u32, ALIGN as u32) }.cast::<u8>();
            if block.is_null() {
                if retried {
                    return Err(format!("a draw takes {size} bytes, more than a batch has"));
                }
                self.submit(&[])?;
                retried = true;
                continue;
            }
            // SAFETY: the block holds size bytes, each part lies inside it
            // and none overlaps another
            let part = |at: usize, bytes: usize| unsafe { std::slice::from_raw_parts_mut(block.add(at), bytes) };
            place_vertices(vertices, (draw.width as f32, height as f32), raise as f32, depth_map, part(vertex_at, vertex_bytes));
            for (out, index) in part(index_at, index_bytes).as_chunks_mut::<4>().0.iter_mut().zip(indices) {
                *out = index.to_ne_bytes();
            }
            command.uniforms = match uniform_at {
                Some(at) => {
                    for (out, word) in part(at, UNIFORM_BYTES).as_chunks_mut::<4>().0.iter_mut().zip(&self.words) {
                        *out = word.to_le_bytes();
                    }
                    self.last_words.clone_from(&self.words);
                    self.uniforms_at = Some(part(at, 0).as_ptr());
                    part(at, 0).as_ptr()
                }
                None => uniforms_kept.unwrap_or(std::ptr::null()),
            };
            command.tables = match tables_at {
                Some(at) => {
                    hw::pack_tables(draw.tables, draw.proctex_tables, draw.fog_table, part(at, TABLES_BYTES).as_chunks_mut::<4>().0);
                    self.tables_at = Some((generation, part(at, 0).as_ptr()));
                    part(at, 0).as_ptr()
                }
                None if tables_read => tables_kept.unwrap_or(std::ptr::null()),
                None => std::ptr::null(),
            };
            command.vertices = part(vertex_at, 0).as_ptr();
            command.indices = part(index_at, 0).as_ptr().cast();
            break;
        }
        command.vertex_count = vertices.len() as u32;
        command.index_count = indices.len() as u32;
        command.uniforms_size = UNIFORM_BYTES as u32;

        // what the draw may change
        let writes = fixed_function(&mut command, r, draw.depth);
        if writes.color {
            self.surfaces[color].drew(rows);
            self.surfaces[color].guard_writes(memory);
            self.overdrawn(color, rows, depth);
        }
        if let Some(depth) = depth.filter(|_| writes.depth) {
            self.surfaces[depth].drew(rows);
            self.overdrawn(depth, rows, Some(color));
            // only depth, which titles read to tell what is in view, see
            // the Vulkan backend
            self.surfaces[depth].guard(memory);
            self.surfaces[depth].guard_writes(memory);
        }
        for index in [Some(color), depth].into_iter().flatten() {
            self.surfaces[index].pending = true;
        }
        self.commands.push(command);
        Ok(())
    }

    /// runs what the batch recorded and writes everything it drew back to
    /// guest memory.
    pub(crate) fn flush<M: GpuMemory>(&mut self, memory: &mut M) -> Result<(), String> {
        let dirty = (0..self.surfaces.len()).filter(|&i| self.surfaces[i].dirty.is_some()).collect();
        self.write_back(memory, dirty)?;
        self.submit(&[])
    }

    /// lets go of what the GPU drew that guest memory lacks, memory having
    /// been put back as a saved state had it.
    pub(crate) fn forget(&mut self) {
        for surface in &mut self.surfaces {
            surface.lose();
        }
    }

    /// makes guest memory right over a range something other than a draw
    /// is about to read or write.
    pub(crate) fn sync<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32) -> Result<(), String> {
        self.sync_where(memory, addr, len, |_| true)
    }

    /// the same over the depth buffers alone, for the CPU reading where a
    /// depth buffer was drawn.
    pub(crate) fn sync_depth<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32) -> Result<(), String> {
        self.sync_where(memory, addr, len, |surface| matches!(surface.kind, Kind::Depth(_)))
    }

    fn sync_where<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32, kept: impl Fn(&Surface) -> bool) -> Result<(), String> {
        let overlapping: Vec<usize> =
            (0..self.surfaces.len()).filter(|&i| kept(&self.surfaces[i]) && self.surfaces[i].overlaps(addr, len)).collect();
        let dirty: Vec<usize> = overlapping.iter().copied().filter(|&i| self.surfaces[i].dirty_overlaps(addr, len)).collect();
        self.write_back(memory, dirty)?;
        for i in overlapping {
            self.surfaces[i].checked = false;
        }
        Ok(())
    }

    /// makes sure guest memory holds what the GPU drew over a range, before
    /// something reads it.
    pub(crate) fn prepare_read<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32) -> Result<(), String> {
        if self.surfaces.iter().any(|s| s.dirty_overlaps(addr, len)) {
            self.sync(memory, addr, len)?;
        }
        Ok(())
    }

    /// a fill is about to write a range, whatever the GPU drew over the part
    /// of a buffer it leaves alone has to come down first. what it drew
    /// under the fill alone the fill makes moot.
    pub(crate) fn before_fill<M: GpuMemory>(&mut self, memory: &mut M, addr: u32, len: u32, _width: u32) -> Result<(), String> {
        let end = addr as u64 + len as u64;
        let partial: Vec<usize> = (0..self.surfaces.len())
            .filter(|&i| {
                let s = &self.surfaces[i];
                s.dirty_overlaps(addr, len) && (s.addr < addr || s.addr as u64 + s.size() as u64 > end) && !s.dirty_within(addr, len)
            })
            .collect();
        self.write_back(memory, partial)
    }

    /// a fill wrote bytes over memory. a buffer it covers with one pixel is
    /// cleared on the GPU, one it covers otherwise or in part takes memory's
    /// bytes at its next use.
    pub(crate) fn filled(&mut self, addr: u32, bytes: &[u8]) -> Result<(), String> {
        let len = bytes.len() as u32;
        let end = addr as u64 + bytes.len() as u64;
        for index in 0..self.surfaces.len() {
            let s = &self.surfaces[index];
            if !s.overlaps(addr, len) {
                continue;
            }
            let size = s.size() as usize;
            if s.addr < addr || s.addr as u64 + size as u64 > end {
                // before_fill wrote back what the GPU drew past the fill,
                // what it drew under it is gone
                let s = &mut self.surfaces[index];
                if s.dirty_within(addr, len) {
                    s.dirty = None;
                    s.guarded = None;
                    s.write_guarded = None;
                }
                s.checked = false;
                continue;
            }
            let offset = (s.addr - addr) as usize;
            let filled = &bytes[offset..offset + size];
            let bpp = s.kind.bytes() as usize;
            // a pattern that does not line up with the pixels leaves them
            // different from each other, uploading handles that
            if !filled.iter().enumerate().take(12).all(|(i, &b)| b == filled[i % bpp]) {
                self.surfaces[index].lose();
                continue;
            }
            let pixel = filled[..bpp].to_vec();
            self.commands.push(clear(s.kind, s.target, &pixel, s.whole()));
            let s = &mut self.surfaces[index];
            s.pending = true;
            s.keep_pixel(&pixel);
            s.dirty = None;
            s.guarded = None;
            s.write_guarded = None;
            s.checked = false;
        }
        Ok(())
    }

    /// display transfers go to the CPU, which syncs what they read and
    /// write first.
    pub(crate) fn display_transfer<M: GpuMemory>(&mut self, _memory: &mut M, _transfer: &Transfer) -> Result<bool, String> {
        Ok(false)
    }

    /// textures are read from guest memory, never copied from a surface.
    pub(crate) fn covers(&self, _texture: &DrawnTexture) -> bool {
        false
    }

    pub(crate) fn holds(&self, _texture: &DrawnTexture) -> bool {
        false
    }

    pub(crate) fn holds_unchanged<M: GpuMemory>(&self, _memory: &mut M, _texture: &DrawnTexture) -> bool {
        false
    }

    /// the screens come from guest memory.
    pub(crate) fn screen(&mut self, _addr: u32, _size: (u32, u32), _stride: u32, _format: ColorFormat, _guest: &[u8]) -> Option<ScreenRef> {
        None
    }

    pub(crate) fn picture(&mut self, _screen: ScreenRef) -> Result<Option<Picture>, String> {
        Ok(None)
    }

    pub(crate) fn screen_image(&mut self, _screen: ScreenRef) -> Result<Option<crate::GpuScreen>, String> {
        Ok(None)
    }

    pub(crate) fn between_image(&mut self, _screen: ScreenRef) -> Result<Option<crate::GpuScreen>, String> {
        Ok(None)
    }

    pub(crate) fn upright_picture(&mut self, _screen: ScreenRef, _between: bool) -> Option<Picture> {
        None
    }

    /// runs the batch with the surfaces given read back, depth through its
    /// color target, and writes them to guest memory.
    fn write_back<M: GpuMemory>(&mut self, memory: &mut M, dirty: Vec<usize>) -> Result<(), String> {
        if dirty.is_empty() {
            return Ok(());
        }
        let mut readback = Vec::with_capacity(dirty.len());
        for &i in &dirty {
            match self.surfaces[i].kind {
                Kind::Color(_) => readback.push(self.surfaces[i].target),
                Kind::Depth(_) => {
                    let (store, _) = self.depth_store(i)?;
                    let s = &self.surfaces[i];
                    self.commands.push(Command { kind: CMD_STORE_DEPTH, color: store, depth: s.target, scissor: s.whole(), ..Command::NONE });
                    readback.push(store);
                }
            }
        }
        self.submit(&readback)?;
        for i in dirty {
            self.store(memory, i);
        }
        Ok(())
    }

    /// writes what the GPU read back of a surface to guest memory, the rows
    /// drawn, keeping what something else wrote over them since.
    fn store<M: GpuMemory>(&mut self, memory: &mut M, index: usize) {
        self.surfaces[index].shadow();
        let s = &self.surfaces[index];
        let (width, height, kind, addr) = (s.width, s.height, s.kind, s.addr);
        let level = match kind {
            Kind::Color(_) => s.level,
            Kind::Depth(_) => match s.store {
                Some((_, level)) => level,
                None => return,
            },
        };
        // only the rows drawn, the others can be another buffer's by now
        let (first, last) = s.dirty.unwrap_or((0, height));
        let row = s.row_bytes() as usize;
        let rows = first as usize * row..last as usize * row;
        let mut bytes = vec![0u8; s.size() as usize];
        // SAFETY: the submit that read the surface back returned, its pixels
        // are the CPU's to read
        let pixels = unsafe { level.rows(height) };
        match kind {
            Kind::Color(format) => color_down(format, pixels, level.pitch, width, height, (first, last), &mut bytes),
            Kind::Depth(sample) => {
                // the stencil the GPU can't give back stays as memory has it
                if sample == 4 {
                    memory.read(addr + rows.start as u32, &mut bytes[rows.clone()]);
                }
                depth_down(sample, pixels, level.pitch, width, height, (first, last), &mut bytes);
            }
        }
        let drawn = bytes[rows.clone()].to_vec();
        // what was written over the buffer since the image last matched it
        // came after the drawing and stays
        if s.shadow.len() == bytes.len() {
            let mut now = vec![0u8; rows.len()];
            memory.read(addr + rows.start as u32, &mut now);
            for ((out, &held), &was) in bytes[rows.clone()].iter_mut().zip(&now).zip(&s.shadow[rows.clone()]) {
                if held != was {
                    *out = held;
                }
            }
        }
        memory.write(addr + rows.start as u32, &bytes[rows.clone()]);
        let surface = &mut self.surfaces[index];
        if surface.shadow.len() == bytes.len() {
            surface.shadow[rows].copy_from_slice(&drawn);
        } else {
            memory.read(addr, &mut bytes);
            surface.shadow = bytes;
        }
        surface.dirty = None;
        surface.guarded = None;
        surface.write_guarded = None;
    }
}

impl Drop for Hardware {
    fn drop(&mut self) {
        // nothing submitted uses them, the batch recorded goes unrun
        self.commands.clear();
        for surface in &self.surfaces {
            surface.destroy();
        }
        for texture in self.textures.values() {
            destroy(texture.handle);
        }
    }
}

/// a command clearing a surface to a pixel of guest memory's.
fn clear(kind: Kind, target: u32, pixel: &[u8], scissor: [i32; 4]) -> Command {
    match kind {
        Kind::Color(format) => Command {
            kind: CMD_CLEAR_COLOR,
            color: target,
            scissor,
            clear_color: format.decode(pixel).map(|c| c as f32 / 255.0),
            ..Command::NONE
        },
        Kind::Depth(sample) => {
            let (depth, stencil) = depth_sample(sample, pixel);
            Command {
                kind: CMD_CLEAR_DEPTH,
                depth: target,
                scissor,
                clear_depth: depth as f32 / 16_777_215.0,
                clear_stencil: stencil,
                clear_flags: 3,
                ..Command::NONE
            }
        }
    }
}

/// a depth sample of guest memory as 24 bits of depth and the stencil.
fn depth_sample(sample: u32, bytes: &[u8]) -> (u32, u8) {
    match sample {
        2 => {
            let d16 = u16::from_le_bytes([bytes[0], bytes[1]]) as u64;
            (((d16 * 0xFF_FFFF + 0x7FFF) / 0xFFFF) as u32, 0)
        }
        3 => (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]), 0),
        _ => (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]), bytes[3]),
    }
}

/// a tiled color buffer of guest memory into RGBA8 pixels, the image's rows
/// bottom first, pitch pixels each.
fn color_up(format: ColorFormat, guest: &[u8], width: u32, height: u32, pixels: &mut [u8], pitch: u32) {
    let bpp = format.bytes_per_pixel();
    for y in 0..height {
        let line = &mut pixels[((height - 1 - y) * pitch * 4) as usize..][..width as usize * 4];
        for (x, out) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let at = morton_offset(x as u32, y, width, bpp as u32) as usize;
            *out = format.decode(&guest[at..at + bpp]);
        }
    }
}

/// rows of memory of RGBA8 pixels back into a tiled color buffer.
fn color_down(format: ColorFormat, pixels: &[u8], pitch: u32, width: u32, height: u32, (first, last): (u32, u32), guest: &mut [u8]) {
    let bpp = format.bytes_per_pixel();
    for y in first..last {
        let line = &pixels[((height - 1 - y) * pitch * 4) as usize..][..width as usize * 4];
        for (x, &rgba) in line.as_chunks::<4>().0.iter().enumerate() {
            let at = morton_offset(x as u32, y, width, bpp as u32) as usize;
            format.encode(rgba, &mut guest[at..at + bpp]);
        }
    }
}

/// a tiled depth buffer of guest memory into the RGBA8 pixels LOAD_DEPTH
/// takes, the depth's low byte in red, the stencil in alpha. gives the
/// first pixel's stencil, and whether every pixel has it.
fn depth_up(sample: u32, guest: &[u8], width: u32, height: u32, pixels: &mut [u8], pitch: u32) -> (u8, bool) {
    let first = depth_sample(sample, &guest[..sample as usize]).1;
    let mut same = true;
    for y in 0..height {
        let line = &mut pixels[((height - 1 - y) * pitch * 4) as usize..][..width as usize * 4];
        for (x, out) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let at = morton_offset(x as u32, y, width, sample) as usize;
            let (depth, stencil) = depth_sample(sample, &guest[at..at + sample as usize]);
            let [r, g, b, _] = depth.to_le_bytes();
            *out = [r, g, b, stencil];
            same &= stencil == first;
        }
    }
    (first, same)
}

/// rows of memory of what STORE_DEPTH left back into a tiled depth buffer,
/// a D24S8 one's stencil left as guest holds it.
fn depth_down(sample: u32, pixels: &[u8], pitch: u32, width: u32, height: u32, (first, last): (u32, u32), guest: &mut [u8]) {
    for y in first..last {
        let line = &pixels[((height - 1 - y) * pitch * 4) as usize..][..width as usize * 4];
        for (x, &[r, g, b, _]) in line.as_chunks::<4>().0.iter().enumerate() {
            let d24 = u32::from_le_bytes([r, g, b, 0]);
            let at = morton_offset(x as u32, y, width, sample) as usize;
            match sample {
                2 => {
                    let d16 = ((d24 as u64 * 0xFFFF + 0x7F_FFFF) / 0xFF_FFFF) as u16;
                    guest[at..at + 2].copy_from_slice(&d16.to_le_bytes());
                }
                3 => guest[at..at + 3].copy_from_slice(&d24.to_le_bytes()[..3]),
                _ => guest[at..at + 3].copy_from_slice(&d24.to_le_bytes()[..3]),
            }
        }
    }
}

/// the vertices the CPU placed as raster.vert takes them, native-endian
/// floats: clip space over the whole target, y = -1 its image's row 0, the
/// window's bottom, a draw into the start of a taller buffer raised by the
/// rows it lacks, with w kept for perspective.
fn place_vertices(vertices: &[Screen], (width, height): (f32, f32), raise: f32, depth_map: DepthMap, out: &mut [u8]) {
    for (out, v) in out.as_chunks_mut::<VERTEX_SIZE>().0.iter_mut().zip(vertices) {
        let w = 1.0 / v.inv_w;
        let x = v.x / width * 2.0 - 1.0;
        let y = (v.y + raise) / height * 2.0 - 1.0;
        let depth = v.z * depth_map.scale + depth_map.offset;
        let (c, t, q, view) = (v.color_over_w, v.texcoords_over_w, v.quaternion_over_w, v.view_over_w);
        let values: [f32; 24] = [
            x * w,
            y * w,
            0.0,
            w,
            c[0] * w,
            c[1] * w,
            c[2] * w,
            c[3] * w,
            t[0][0] * w,
            t[0][1] * w,
            t[1][0] * w,
            t[1][1] * w,
            t[2][0] * w,
            t[2][1] * w,
            depth,
            0.0,
            q[0] * w,
            q[1] * w,
            q[2] * w,
            q[3] * w,
            view[0] * w,
            view[1] * w,
            view[2] * w,
            v.texcoord0_w_over_w * w,
        ];
        for (bytes, value) in out.as_chunks_mut::<4>().0.iter_mut().zip(values) {
            *bytes = value.to_ne_bytes();
        }
    }
}

/// a unit's sampler, as the Vulkan backend makes it: the border is the
/// shader's to draw, past the edge it clamps.
fn sampler(bound: &BoundTexture) -> Sampler {
    let wrap = |wrap: Wrap| match wrap {
        Wrap::ClampToEdge => 0,
        Wrap::ClampToBorder => 1,
        Wrap::Repeat => 2,
        Wrap::MirroredRepeat => 3,
    };
    let mut sampler = Sampler { mag_linear: bound.linear as u8, wrap_s: wrap(bound.wrap_s), wrap_t: wrap(bound.wrap_t), ..Sampler::default() };
    match bound.levels {
        // one size is only ever magnified
        0 | 1 => sampler.min_linear = bound.linear as u8,
        levels => {
            let lod = bound.lod;
            let largest = levels.min(MAX_LEVELS as u32) - 1;
            sampler.min_linear = lod.linear as u8;
            sampler.mip = if lod.between { 2 } else { 1 };
            sampler.min_lod = (lod.min as u32).min(largest) as f32;
            sampler.max_lod = (lod.max as u32).min(largest) as f32;
            sampler.lod_bias = (lod.bias as f32 / 256.0).clamp(-MOST_BIAS, MOST_BIAS);
        }
    }
    sampler
}

/// what a draw writes, by fixed_function.
struct Writes {
    color: bool,
    depth: bool,
}

/// the tests, the stencil, blending, the logic op and the color mask, the
/// PICA's encodings, as the Vulkan backend works its pipeline state out of
/// the registers. depth is the draw's depth buffer and its bytes per sample.
fn fixed_function(command: &mut Command, r: &[u32], depth: Option<(u32, u32)>) -> Writes {
    let mask = r[REG_DEPTH_COLOR_MASK];
    let writable = r[REG_DEPTH_STENCIL_WRITE] != 0;
    let color_mask = if r[REG_COLOR_BUFFER_WRITE] != 0 { (mask >> 8) & 0xF } else { 0 };
    let depth_test = mask & 1 != 0;
    let depth_write = writable && mask & (1 << 12) != 0;
    // with the test off the PICA still writes depth, as a test that always
    // passes does
    command.depth_test = (depth.is_some() && (depth_test || depth_write)) as u8;
    command.depth_func = if depth_test { ((mask >> 4) & 7) as u8 } else { 1 };
    command.depth_write = (depth.is_some() && depth_write) as u8;
    // the stencil's operations apply with its test's bit clear too, the
    // test then passing always, see Stencil::read
    let stencil_test = depth.is_some_and(|(_, bytes)| bytes == 4) && super::Stencil::read(r, writable).is_some();
    let test = match r[REG_STENCIL_TEST] & 1 {
        0 => r[REG_STENCIL_TEST] & !0x70 | 0x10,
        _ => r[REG_STENCIL_TEST],
    };
    let op = r[REG_STENCIL_OP];
    command.stencil_test = stencil_test as u8;
    command.stencil_func = ((test >> 4) & 7) as u8;
    command.stencil_ref = (test >> 16) as u8;
    command.stencil_mask = (test >> 24) as u8;
    command.stencil_write_mask = if writable { (test >> 8) as u8 } else { 0 };
    command.stencil_fail = (op & 7) as u8;
    command.stencil_zfail = ((op >> 4) & 7) as u8;
    command.stencil_zpass = ((op >> 8) & 7) as u8;
    if r[REG_COLOR_OPERATION] & 0x100 != 0 {
        let config = r[REG_BLEND_FUNC];
        // equations past max add, factor 15 is one
        let equation = |raw: u32| if raw & 7 > 4 { 0 } else { (raw & 7) as u8 };
        let factor = |raw: u32| if raw & 0xF == 15 { 1 } else { (raw & 0xF) as u8 };
        command.blend_enable = 1;
        command.blend_eq_rgb = equation(config);
        command.blend_eq_alpha = equation(config >> 8);
        command.blend_src_rgb = factor(config >> 16);
        command.blend_dst_rgb = factor(config >> 20);
        command.blend_src_alpha = factor(config >> 24);
        command.blend_dst_alpha = factor(config >> 28);
    }
    command.blend_color = r[REG_BLEND_COLOR];
    if let Some(op) = LogicOp::read(r) {
        command.logic_op_enable = 1;
        command.logic_op = op as u8;
    }
    command.color_mask = color_mask as u8;
    Writes { color: color_mask != 0, depth: depth.is_some() && (depth_write || (stencil_test && writable)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fog;
    use crate::lighting::Tables;
    use crate::proctex;

    /// guest memory, a plain run of bytes from address 0.
    struct Ram(Vec<u8>);

    impl GpuMemory for Ram {
        fn read(&mut self, addr: u32, out: &mut [u8]) {
            out.copy_from_slice(&self.0[addr as usize..addr as usize + out.len()]);
        }

        fn write(&mut self, addr: u32, data: &[u8]) {
            self.0[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        }

        fn slice(&mut self, addr: u32, len: usize) -> Option<&[u8]> {
            self.0.get(addr as usize..addr as usize + len)
        }
    }

    fn hardware() -> Hardware {
        ffi::with(|fake| {
            *fake = ffi::Fake::default();
            fake.available = true;
        });
        Hardware::new().unwrap()
    }

    /// bytes that differ from pixel to pixel and from row to row.
    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
    }

    #[test]
    fn the_layouts_match_the_header() {
        assert_eq!(VERTEX_SIZE, 96);
        assert_eq!(TABLES_BYTES, 61440);
        assert_eq!(UNIFORM_BYTES, 1632);
        assert_eq!(std::mem::size_of::<Sampler>(), 20);
    }

    #[test]
    fn the_uniforms_go_as_little_endian_words_where_raster_frag_reads_them() {
        let mut r = vec![0u32; 0x300];
        r[0x0C0] = 0x1122_3344;
        r[REG_TEXTURE_CONFIG] = 0x0000_2407;
        r[REG_ALPHA_TEST] = 0x0000_8051;
        r[fog::REG_COLOR] = 0x00AB_CDEF;
        let depth_map = DepthMap { scale: -1.0, offset: 0.5, w_buffer: true };
        let mut words = Vec::new();
        let (config, flags) = hw::pack_uniforms(&r, 0b101, depth_map, false, None, &mut words);
        assert_eq!(words.len(), UNIFORM_WORDS);
        assert_eq!(config, 0x2405);
        assert_eq!(flags, 1);
        // tev[0].x, misc, flags, then the fog's color last but three
        assert_eq!(words[0], 0x1122_3344);
        assert_eq!(&words[48..52], &[r[0x0E0], r[0x0FD], 0x8051, 0x2405]);
        assert_eq!(&words[64..68], &[1, 0, (-1.0f32).to_bits(), 0.5f32.to_bits()]);
        assert_eq!(words[UNIFORM_WORDS - 4], 0x00AB_CDEF);
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        assert_eq!(&bytes[..4], &[0x44, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn the_tables_go_in_the_order_raster_frag_reads_them() {
        let mut fog = fog::Table::default();
        let entries: Vec<[f32; 2]> = (0..fog::ENTRIES).map(|i| [i as f32, -1.0]).collect();
        assert!(fog.restore(&entries));
        let mut out = vec![[0u8; 4]; TABLES_BYTES / 4];
        hw::pack_tables(&Tables::default(), &proctex::Tables::default(), &fog, &mut out);
        // FOG_TABLE in raster.frag, in vec2s
        let fog_table = 24 * 256 + 3 * 128 + 2 * 256 + 2 * 256;
        assert_eq!((fog_table + fog::ENTRIES) * 2, out.len());
        for (i, entry) in out[fog_table * 2..].as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(entry[0], (i as f32).to_le_bytes());
            assert_eq!(entry[1], (-1.0f32).to_le_bytes());
        }
    }

    #[test]
    fn color_buffers_go_up_and_come_back_the_same() {
        use ColorFormat::*;
        for format in [Rgba8, Rgb8, Rgb565, Rgb5A1, Rgba4] {
            let (width, height) = (16, 24);
            let bpp = format.bytes_per_pixel();
            let guest = pattern((width * height) as usize * bpp, 7);
            let pitch = width + 5;
            let mut pixels = vec![0u8; (pitch * height * 4) as usize];
            color_up(format, &guest, width, height, &mut pixels, pitch);
            // memory's first row is the image's last
            let at = morton_offset(3, 0, width, bpp as u32) as usize;
            let last = ((height - 1) * pitch * 4 + 3 * 4) as usize;
            assert_eq!(pixels[last..last + 4], format.decode(&guest[at..at + bpp]));
            let mut back = vec![0u8; guest.len()];
            color_down(format, &pixels, pitch, width, height, (0, height), &mut back);
            assert_eq!(back, guest, "{format:?}");
        }
    }

    #[test]
    fn depth_buffers_go_up_and_come_back_the_same() {
        for sample in [2, 3, 4] {
            let (width, height) = (8, 16);
            let guest = pattern((width * height * sample) as usize, 3);
            let pitch = width + 3;
            let mut pixels = vec![0u8; (pitch * height * 4) as usize];
            let (_, same) = depth_up(sample, &guest, width, height, &mut pixels, pitch);
            assert!(!same || sample != 4);
            // STORE_DEPTH gives no stencil back
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel[3] = 0;
            }
            let mut back = if sample == 4 { guest.clone() } else { vec![0u8; guest.len()] };
            depth_down(sample, &pixels, pitch, width, height, (0, height), &mut back);
            assert_eq!(back, guest, "{sample} bytes a sample");
        }
    }

    #[test]
    fn a_buffer_drawn_on_the_gpu_comes_back_to_memory() {
        let mut hardware = hardware();
        let (width, height) = (16, 16);
        let format = ColorFormat::Rgb565;
        let guest = pattern((width * height * 2) as usize, 1);
        let mut memory = Ram(guest.clone());
        let index = hardware.surface_rows(&mut memory, 0, (width, height), Kind::Color(format), None).unwrap();
        let target = hardware.surfaces[index].target;
        // the GPU draws a pixel in the window's bottom row, the image's first
        ffi::with(|fake| fake.set_pixel(target, 5, 0, [0xFF, 0, 0xFF, 0xFF]));
        hardware.surfaces[index].drew((8, 16));
        hardware.sync(&mut memory, 0, width * height * 2).unwrap();
        let at = morton_offset(5, height - 1, width, 2) as usize;
        let mut expected = guest.clone();
        format.encode([0xFF, 0, 0xFF, 0xFF], &mut expected[at..at + 2]);
        assert_eq!(memory.0, expected);
        assert!(hardware.surfaces[index].dirty.is_none());
    }

    #[test]
    fn depth_goes_up_through_a_texture_and_down_through_color() {
        let mut hardware = hardware();
        let (width, height) = (8, 8);
        let mut guest = pattern((width * height * 4) as usize, 9);
        // one stencil value, which a clear can give
        for sample in guest.as_chunks_mut::<4>().0 {
            sample[3] = 0x5A;
        }
        let mut memory = Ram(guest.clone());
        let index = hardware.surface_rows(&mut memory, 0, (width, height), Kind::Depth(4), None).unwrap();
        let target = hardware.surfaces[index].target;
        hardware.surfaces[index].drew((0, height));
        hardware.sync_depth(&mut memory, 0, width * height * 4).unwrap();
        assert_eq!(memory.0, guest);
        let depth = ffi::with(|fake| fake.depth(target));
        assert!(depth.iter().all(|sample| sample >> 24 == 0x5A));
        // the GPU changes a sample, which comes down alone
        ffi::with(|fake| fake.set_depth(target, 2, 0, 0x5A12_3456));
        hardware.surfaces[index].drew((0, height));
        hardware.sync_depth(&mut memory, 0, width * height * 4).unwrap();
        let at = morton_offset(2, height - 1, width, 4) as usize;
        assert_eq!(memory.0[at..at + 4], [0x56, 0x34, 0x12, 0x5A]);
    }

    #[test]
    fn a_fill_over_a_buffer_clears_it_on_the_gpu() {
        let mut hardware = hardware();
        let (width, height) = (8, 8);
        let mut memory = Ram(pattern((width * height * 4) as usize, 5));
        let index = hardware.surface_rows(&mut memory, 0, (width, height), Kind::Color(ColorFormat::Rgba8), None).unwrap();
        hardware.surfaces[index].drew((0, 8));
        let fill = [0x10, 0x20, 0x30, 0x40].repeat((width * height) as usize);
        hardware.before_fill(&mut memory, 0, fill.len() as u32, 4).unwrap();
        memory.0.copy_from_slice(&fill);
        hardware.filled(0, &fill).unwrap();
        assert!(hardware.surfaces[index].dirty.is_none(), "what the GPU drew is gone");
        // memory as the fill left it, which the next draw finds unchanged
        let index = hardware.surface_rows(&mut memory, 0, (width, height), Kind::Color(ColorFormat::Rgba8), None).unwrap();
        hardware.flush(&mut memory).unwrap();
        let target = hardware.surfaces[index].target;
        let pixels = ffi::with(|fake| fake.pixels(target));
        assert!(pixels.as_chunks::<4>().0.iter().all(|pixel| *pixel == [0x40, 0x30, 0x20, 0x10]));
        let clears = ffi::with(|fake| fake.submitted.iter().filter(|c| c.kind == CMD_CLEAR_COLOR).count());
        assert_eq!(clears, 1);
    }

    #[test]
    fn memory_of_one_pixel_goes_up_as_a_clear() {
        let mut hardware = hardware();
        let mut memory = Ram([0x34, 0x12, 0x00, 0x77].repeat(64));
        hardware.surface_rows(&mut memory, 0, (8, 8), Kind::Depth(4), None).unwrap();
        hardware.flush(&mut memory).unwrap();
        let command = ffi::with(|fake| fake.submitted[0]);
        assert_eq!(command.kind, CMD_CLEAR_DEPTH);
        assert_eq!((command.clear_flags, command.clear_stencil), (3, 0x77));
        assert_eq!((command.clear_depth as f64 * 16_777_215.0).round() as u32, 0x1234);
    }

    #[test]
    fn a_draw_goes_into_its_buffers_with_the_state_of_the_registers() {
        let mut hardware = hardware();
        let (width, height) = (16, 16);
        let mut memory = Ram(vec![0; (width * height * 8) as usize]);
        let mut r = vec![0u32; 0x300];
        r[REG_COLOR_BUFFER_WRITE] = 1;
        r[REG_DEPTH_STENCIL_WRITE] = 1;
        // depth test less than, writing depth and every color
        r[REG_DEPTH_COLOR_MASK] = 1 | 4 << 4 | 0xF << 8 | 1 << 12;
        r[REG_COLOR_OPERATION] = 0x100;
        // add, src alpha and one minus it
        r[REG_BLEND_FUNC] = 6 << 16 | 7 << 20 | 15 << 24 | 0 << 28;
        let vertex = |x: f32, y: f32| Screen {
            x,
            y,
            z: -0.5,
            inv_w: 0.5,
            color_over_w: [0.5; 4],
            texcoords_over_w: [[0.0; 2]; 3],
            quaternion_over_w: [0.0; 4],
            view_over_w: [0.0; 3],
            texcoord0_w_over_w: 0.0,
        };
        let vertices = [vertex(0.0, 0.0), vertex(16.0, 0.0), vertex(0.0, 16.0)];
        let (tables, procedural, fog) = (Tables::default(), proctex::Tables::default(), fog::Table::default());
        let textures = [None, None, None];
        let draw = Draw {
            registers: &r,
            target: 0,
            format: ColorFormat::Rgba8,
            width,
            height,
            scissor: [0, 0, 16, 8],
            depth: Some((width * height * 4, 4)),
            depth_map: DepthMap { scale: -1.0, offset: 0.0, w_buffer: false },
            geometry: Geometry::Placed { vertices: &vertices, indices: &[0, 1, 2] },
            textures: &textures,
            lighting: None,
            tables: &tables,
            proctex: false,
            proctex_tables: &procedural,
            fog: false,
            fog_table: &fog,
        };
        hardware.draw(&mut memory, &draw).unwrap();
        let command = *hardware.commands.last().unwrap();
        assert_eq!((command.kind, command.vertex_count, command.index_count), (CMD_DRAW, 3, 3));
        assert_eq!(command.scissor, [0, 0, 16, 8]);
        assert_eq!((command.depth_test, command.depth_func, command.depth_write, command.color_mask), (1, 4, 1, 0xF));
        assert_eq!((command.blend_enable, command.blend_src_rgb, command.blend_dst_rgb, command.blend_src_alpha), (1, 6, 7, 1));
        assert_eq!(command.stencil_test, 0);
        assert!(command.tables.is_null());
        assert_eq!(command.shader, 0);
        // the window's bottom rows are memory's last
        let color = hardware.surfaces.iter().find(|s| matches!(s.kind, Kind::Color(_))).unwrap();
        assert_eq!(color.dirty, Some((8, 16)));
        hardware.flush(&mut memory).unwrap();
        let bytes = ffi::with(|fake| fake.vertices[0].clone());
        let floats: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_ne_bytes(*b)).collect();
        // x, y, z and w of the second vertex, clip space times w, then the
        // mapped depth
        assert_eq!(&floats[24..28], &[2.0, -2.0, 0.0, 2.0]);
        assert_eq!(floats[24 + 14], 0.5);
    }

    #[test]
    fn a_batch_out_of_room_is_submitted_and_the_draw_staged_again() {
        let mut hardware = hardware();
        // the first draw's 2560 bytes fit, not the second's 768 beside them
        ffi::with(|fake| fake.budget = Some(3000));
        let mut memory = Ram(vec![0; 8 * 8 * 4]);
        hardware.surface_rows(&mut memory, 0, (8, 8), Kind::Color(ColorFormat::Rgba8), None).unwrap();
        let fill = vec![0u8; 8 * 8 * 4];
        hardware.filled(0, &fill).unwrap();
        let mut r = vec![0u32; 0x300];
        r[REG_COLOR_BUFFER_WRITE] = 1;
        r[REG_DEPTH_COLOR_MASK] = 0xF << 8;
        let vertex = Screen {
            x: 1.0,
            y: 1.0,
            z: 0.0,
            inv_w: 1.0,
            color_over_w: [1.0; 4],
            texcoords_over_w: [[0.0; 2]; 3],
            quaternion_over_w: [0.0; 4],
            view_over_w: [0.0; 3],
            texcoord0_w_over_w: 0.0,
        };
        let vertices = [vertex; 3];
        let (tables, procedural, fog) = (Tables::default(), proctex::Tables::default(), fog::Table::default());
        let textures = [None, None, None];
        let draw = Draw {
            registers: &r,
            target: 0,
            format: ColorFormat::Rgba8,
            width: 8,
            height: 8,
            scissor: [0, 0, 8, 8],
            depth: None,
            depth_map: DepthMap { scale: -1.0, offset: 0.0, w_buffer: false },
            geometry: Geometry::Placed { vertices: &vertices, indices: &[0, 1, 2] },
            textures: &textures,
            lighting: None,
            tables: &tables,
            proctex: false,
            proctex_tables: &procedural,
            fog: false,
            fog_table: &fog,
        };
        hardware.draw(&mut memory, &draw).unwrap();
        hardware.draw(&mut memory, &draw).unwrap();
        assert_eq!(ffi::with(|fake| fake.submits), 1);
        assert_eq!(hardware.commands.len(), 1, "the second draw starts the next batch");
        assert!(hardware.commands[0].tables.is_null() && !hardware.commands[0].uniforms.is_null());
        // a draw past what a batch has fails, for the software path
        ffi::with(|fake| fake.budget = Some(512));
        assert!(hardware.draw(&mut memory, &draw).is_err());
    }

    #[test]
    fn handles_go_when_the_renderer_does() {
        let mut hardware = hardware();
        let mut memory = Ram(pattern(16 * 16 * 4, 2));
        hardware.surface_rows(&mut memory, 0, (16, 8), Kind::Depth(4), None).unwrap();
        hardware.surfaces[0].drew((0, 8));
        hardware.flush(&mut memory).unwrap();
        assert_eq!(ffi::with(|fake| fake.live()), 3, "the depth target, its texture and its color target");
        drop(hardware);
        assert_eq!(ffi::with(|fake| fake.live()), 0);
    }
}
