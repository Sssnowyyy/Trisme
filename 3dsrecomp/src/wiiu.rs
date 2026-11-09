//! the title's code for the Wii U: the same C build and port write,
//! compiled with devkitPPC's powerpc-eabi-gcc into a static librecomp.a a
//! Wii U program links in, along with info.json, which says what the program
//! needs to know about it. see docs/wiiu.md.
//!
//! the Wii U's Espresso is a big-endian PowerPC, and recomp.h swaps every
//! guest halfword and word there, guest memory being little-endian bytes
//! whatever the host.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::build::{self, Event};
use crate::compile::{self, Compiler};
use crate::{abi, Mods};

/// what powerpc-eabi-gcc compiles the code with. the Wii U's CPU, an
/// Espresso, runs code for the 750; -mlongcall because the code of a big
/// game is tens of MiB and a PowerPC branch reaches 32 MiB either way; one
/// section per function and per table so that the linker can drop what
/// nothing uses; and no -fPIC, an RPX is linked statically. the rest is
/// compile::FLAGS, which the host gets too: floating point rounds exactly the
/// way the interpreter does, so nothing may be fused into a multiply-add,
/// which gcc does on PowerPC by default.
pub const FLAGS: &[&str] = &[
    "-mcpu=750",
    "-meabi",
    "-mhard-float",
    "-mlongcall",
    "-ffunction-sections",
    "-fdata-sections",
    "-fno-strict-aliasing",
    "-fvisibility=hidden",
];

/// the symbols the archive has for the program, as C declares them, which
/// it hands to zakuro::Linked::new and with_origins. recomp_natives is there
/// only when the title has divisions natives.rs runs on the host.
pub const SYMBOLS: &[(&str, &str)] = &[
    ("recomp_abi", "const uint32_t recomp_abi"),
    ("recomp_generation", "const uint32_t recomp_generation"),
    ("recomp_entry_count", "const uint32_t recomp_entry_count"),
    ("recomp_entries", "const Entry recomp_entries[]"),
    ("recomp_module_count", "const uint32_t recomp_module_count"),
    ("recomp_modules", "const Module recomp_modules[]"),
    ("recomp_origins", "const Origins recomp_origins[]"),
];

/// the symbol natives.c adds.
pub const NATIVES: (&str, &str) = ("recomp_natives", "uint8_t recomp_natives");

/// how many objects go to ar at a time, which keeps a command line short
/// whatever the game.
const BATCH: usize = 256;

/// devkitPPC's tools, found once.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub gcc: Compiler,
    pub ar: PathBuf,
    pub nm: PathBuf,
}

impl Toolchain {
    /// powerpc-eabi-gcc from WIIU_CC, $DEVKITPPC/bin, devkitPro's usual
    /// place or the PATH, the first that runs, and its ar and nm beside it.
    pub fn find() -> Result<Toolchain, String> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(named) = std::env::var_os("WIIU_CC").filter(|value| !value.is_empty()) {
            candidates.push(named.into());
        }
        if let Some(devkitppc) = std::env::var_os("DEVKITPPC").filter(|value| !value.is_empty()) {
            candidates.push(Path::new(&devkitppc).join("bin/powerpc-eabi-gcc"));
        }
        if let Some(devkitpro) = std::env::var_os("DEVKITPRO").filter(|value| !value.is_empty()) {
            candidates.push(Path::new(&devkitpro).join("devkitPPC/bin/powerpc-eabi-gcc"));
        }
        candidates.push("/opt/devkitpro/devkitPPC/bin/powerpc-eabi-gcc".into());
        candidates.push("powerpc-eabi-gcc".into());
        let gcc = candidates
            .into_iter()
            .map(|program| Compiler::new(program, &[]))
            .find(Compiler::runs)
            .ok_or("there is no powerpc-eabi-gcc, install devkitPPC from devkitPro, or name it in WIIU_CC or DEVKITPPC")?;
        let sibling = |tool: &str| -> PathBuf {
            let name = gcc.program.file_name().map(|name| name.to_string_lossy().replace("gcc", tool));
            match (gcc.program.parent().filter(|dir| !dir.as_os_str().is_empty()), name) {
                (Some(dir), Some(name)) if dir.join(&name).exists() => dir.join(name),
                _ => format!("powerpc-eabi-{tool}").into(),
            }
        };
        Ok(Toolchain { ar: sibling("ar"), nm: sibling("nm"), gcc })
    }
}

/// what write_wiiu_code made.
#[derive(Debug, Clone)]
pub struct CodeInfo {
    /// the title's name, from its exheader.
    pub title: String,
    /// its title id, the NCCH's program id, which is what Zakuro's
    /// Linked::new takes.
    pub program_id: u64,
    /// dir/code, where the C, the objects and the archive are.
    pub code: PathBuf,
    /// dir/code/librecomp.a.
    pub library: PathBuf,
    /// dir/code/info.json.
    pub info: PathBuf,
    /// recomp.h's RECOMP_ABI and 3dsrecomp's generation, which the archive
    /// holds as recomp_abi and recomp_generation too.
    pub abi: u32,
    pub generation: u32,
    pub entry_count: u32,
    pub module_count: u32,
    /// whether the archive has recomp_natives.
    pub natives: bool,
    pub sources: usize,
    /// the C written, and the archive's size, in bytes.
    pub c_bytes: u64,
    pub library_bytes: u64,
    pub took: Duration,
}

/// compile for size (-Os) rather than speed (-O2). recompiled code comes to
/// some 20 bytes of PowerPC for each byte of ARM at -O2, and -Os takes a
/// quarter off that, which a big game needs to fit the Wii U's code area.
static OPTIMIZE_FOR_SIZE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_optimize_for_size(size: bool) {
    OPTIMIZE_FOR_SIZE.store(size, std::sync::atomic::Ordering::Relaxed);
}

fn optimization() -> &'static [&'static str] {
    if OPTIMIZE_FOR_SIZE.load(std::sync::atomic::Ordering::Relaxed) { &["-Os"] } else { &[] }
}

/// writes the title at rom as C in dir/code, the overrides with it, compiles
/// it for the Wii U into dir/code/librecomp.a and writes dir/code/info.json.
pub fn write_wiiu_code(rom: &Path, dir: &Path, overrides: Option<&Path>) -> Result<CodeInfo, String> {
    write_wiiu_code_with(rom, dir, overrides, Mods::default(), &|_| {})
}

/// the same with a mod's code and files in place of the title's, telling
/// events what happens along the way the way build does.
pub fn write_wiiu_code_with(
    rom: &Path,
    dir: &Path,
    overrides: Option<&Path>,
    mods: Mods,
    events: &(dyn Fn(Event) + Sync),
) -> Result<CodeInfo, String> {
    let tools = Toolchain::find()?;
    let code = dir.join("code");
    let generated = build::generate(rom, |_| code.clone(), overrides, mods, events)?;
    let start = Instant::now();
    let progress = |done, total| {
        events(Event::Compiled { done, total });
        true
    };
    let mut flags: Vec<&str> = compile::FLAGS.to_vec();
    flags.extend_from_slice(FLAGS);
    // the last -O wins
    flags.extend_from_slice(optimization());
    let objects = compile::objects_with(&tools.gcc, &code, &generated.sources, &flags, &progress, false)?;
    let library = code.join("librecomp.a");
    archive(&tools.ar, &code, &objects, &library)?;
    let took = start.elapsed();
    events(Event::Built { library: library.clone(), took });

    let defined = symbols(&tools.nm, &library)?;
    let missing: Vec<&str> = SYMBOLS.iter().map(|(name, _)| *name).filter(|name| !defined.iter().any(|d| d == name)).collect();
    if !missing.is_empty() {
        return Err(format!("{} lacks {}", library.display(), missing.join(", ")));
    }
    let tables = std::fs::read_to_string(code.join("entries.c")).map_err(|e| format!("could not read entries.c, {e}"))?;
    let c_bytes = generated.sources.iter().chain(["recomp.h".to_owned(), "overrides.h".to_owned()].iter())
        .filter_map(|source| std::fs::metadata(code.join(source)).ok())
        .map(|m| m.len())
        .sum();
    let info = CodeInfo {
        title: generated.title.exheader.title.clone(),
        program_id: generated.title.program_id(),
        info: code.join("info.json"),
        abi: abi::ABI,
        generation: abi::GENERATION,
        entry_count: count(&tables, "recomp_entry_count").unwrap_or(0),
        module_count: count(&tables, "recomp_module_count").unwrap_or(0),
        natives: defined.iter().any(|d| d == NATIVES.0),
        sources: generated.sources.len(),
        c_bytes,
        library_bytes: std::fs::metadata(&library).map_or(0, |m| m.len()),
        took,
        code,
        library,
    };
    std::fs::write(&info.info, info_json(&info, &flags))
        .map_err(|e| format!("could not write {}, {e}", info.info.display()))?;
    Ok(info)
}

/// puts objects, paths inside code, into a static library, a batch at a
/// time, from scratch.
fn archive(ar: &Path, code: &Path, objects: &[PathBuf], library: &Path) -> Result<(), String> {
    if library.exists() {
        std::fs::remove_file(library).map_err(|e| format!("could not replace {}, {e}", library.display()))?;
    }
    let library = std::path::absolute(library).map_err(|e| format!("archiving failed, {e}"))?;
    for batch in objects.chunks(BATCH) {
        let names = batch.iter().map(|object| object.strip_prefix(code).unwrap_or(object));
        let status = Command::new(ar).current_dir(code).arg("rcs").arg(&library).args(names).status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => return Err(format!("archiving with {} failed, {status}", ar.display())),
            Err(error) => return Err(format!("archiving with {} failed, {error}", ar.display())),
        }
    }
    Ok(())
}

/// the symbols library defines, outside any object's own.
fn symbols(nm: &Path, library: &Path) -> Result<Vec<String>, String> {
    let output = Command::new(nm)
        .args(["-g", "--defined-only"])
        .arg(library)
        .output()
        .map_err(|e| format!("could not run {}, {e}", nm.display()))?;
    if !output.status.success() {
        return Err(format!("{} failed on {}", nm.display(), library.display()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields.len() == 3).then(|| fields[2].to_owned())
        })
        .collect())
}

/// a count entries.c sets, `RECOMP_EXPORT const uint32_t name = N;`.
fn count(tables: &str, name: &str) -> Option<u32> {
    let at = tables.find(&format!("uint32_t {name} = "))?;
    let rest = &tables[at..];
    let value = rest[rest.find('=')? + 1..rest.find(';')?].trim().trim_end_matches('u');
    value.parse().ok()
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// info.json, what a Wii U program linking the archive needs to know.
fn info_json(info: &CodeInfo, flags: &[&str]) -> String {
    let mut symbols: Vec<(&str, &str)> = SYMBOLS.to_vec();
    if info.natives {
        symbols.push(NATIVES);
    }
    let symbols: Vec<String> =
        symbols.iter().map(|(name, ty)| format!("    {}: {}", json_string(name), json_string(ty))).collect();
    let flags: Vec<String> = flags.iter().map(|flag| json_string(flag)).collect();
    format!(
        "{{\n  \"title_name\": {},\n  \"title_id\": \"{:016X}\",\n  \"program_id\": \"{:016X}\",\n  \"target\": \"wiiu\",\n  \
         \"endian\": \"big\",\n  \"recomp_abi\": {},\n  \"recomp_generation\": {},\n  \"recomp_entry_count\": {},\n  \
         \"recomp_module_count\": {},\n  \"natives\": {},\n  \"library\": \"librecomp.a\",\n  \"sources\": {},\n  \
         \"symbols\": {{\n{}\n  }},\n  \"cflags\": [{}],\n  \"link\": [\"-lm\"]\n}}\n",
        json_string(&info.title),
        info.program_id,
        info.program_id,
        info.abi,
        info.generation,
        info.entry_count,
        info.module_count,
        info.natives,
        info.sources,
        symbols.join(",\n"),
        flags.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_come_from_the_tables() {
        let tables = "RECOMP_EXPORT const uint32_t recomp_entry_count = 12;\nRECOMP_EXPORT const uint32_t recomp_module_count = 0;\n";
        assert_eq!(count(tables, "recomp_entry_count"), Some(12));
        assert_eq!(count(tables, "recomp_module_count"), Some(0));
        assert_eq!(count(tables, "recomp_abi"), None);
    }

    #[test]
    fn strings_are_escaped_for_json() {
        assert_eq!(json_string("a \"b\"\\\n"), "\"a \\\"b\\\"\\\\\\u000a\"");
    }

    /// recomp.h compiles for the Wii U, big-endian, and its loads and
    /// stores of guest memory become the byte-reversing ones.
    #[test]
    fn the_header_swaps_guest_memory_on_the_wii_u() {
        let Ok(tools) = Toolchain::find() else {
            eprintln!("no powerpc-eabi-gcc, the Wii U went unchecked");
            return;
        };
        let dir = std::env::temp_dir().join(format!("3dsrecomp-wiiu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recomp.h"), abi::HEADER).unwrap();
        let source = r#"#include "recomp.h"
_Static_assert(RECOMP_BIG_ENDIAN, "big-endian");
_Static_assert(sizeof(Context) == 120 && sizeof(Piece) == 16 && sizeof(Origins) == 32, "layout");
uint32_t r32(Context *c, uint32_t a) { return mem_read32(c, a); }
uint32_t r16(Context *c, uint32_t a) { return mem_read16(c, a); }
void w32(Context *c, uint32_t a, uint32_t v) { mem_write32(c, a, v); }
void w16(Context *c, uint32_t a, uint32_t v) { mem_write16(c, a, (uint16_t)v); }
uint32_t l32(const uint8_t *at) { return load32(at); }
void s32(uint8_t *at, uint32_t v) { store32(at, v); }
"#;
        std::fs::write(dir.join("t.c"), source).unwrap();
        let output = Command::new(&tools.gcc.program)
            .args(compile::FLAGS)
            .args(FLAGS)
            .args(optimization())
            .args(["-S", "-o", "-", "t.c"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let assembly = String::from_utf8_lossy(&output.stdout);
        for instruction in ["lwbrx", "lhbrx", "stwbrx", "sthbrx"] {
            assert!(assembly.contains(instruction), "no {instruction} in\n{assembly}");
        }
    }
}
