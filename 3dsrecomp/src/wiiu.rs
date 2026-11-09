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

/// how many objects go to ar at a time, and how long their names may be
/// together, which keeps a command line well inside the 32767 characters
/// Windows allows whatever the game and its overrides' names. the names are
/// relative to the code folder, code123.o and hand/name.o.
const BATCH: usize = 256;
const BATCH_CHARS: usize = 16 << 10;

/// devkitPPC's tools, found once.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub gcc: Compiler,
    pub ar: PathBuf,
    pub nm: PathBuf,
}

impl Toolchain {
    /// powerpc-eabi-gcc from WIIU_CC, devkitppc()/bin, devkitpro()'s
    /// devkitPPC or the PATH, the first that runs, and its ar and nm beside
    /// it.
    pub fn find() -> Result<Toolchain, String> {
        let exe = std::env::consts::EXE_SUFFIX;
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(named) = std::env::var_os("WIIU_CC").filter(|value| !value.is_empty()) {
            candidates.push(host_path(&named.to_string_lossy()));
        }
        candidates.push(devkitppc().join("bin").join(format!("powerpc-eabi-gcc{exe}")));
        candidates.push(devkitpro().join("devkitPPC").join("bin").join(format!("powerpc-eabi-gcc{exe}")));
        candidates.push("powerpc-eabi-gcc".into());
        candidates.dedup();
        let gcc = candidates
            .into_iter()
            .map(|program| Compiler::new(program, &[]))
            .find(Compiler::runs)
            .ok_or("there is no powerpc-eabi-gcc, install devkitPPC from devkitPro, or name it in WIIU_CC, DEVKITPPC or DEVKITPRO")?;
        let sibling = |tool: &str| -> PathBuf {
            // the compiler's name with tool in place of gcc, .exe or not
            // (Windows runs powerpc-eabi-gcc as powerpc-eabi-gcc.exe)
            let name = gcc.program.file_name().map(|name| name.to_string_lossy().replace("gcc", tool));
            if let (Some(dir), Some(name)) = (gcc.program.parent().filter(|dir| !dir.as_os_str().is_empty()), name) {
                for name in [name.clone(), format!("{name}{exe}")] {
                    if dir.join(&name).is_file() {
                        return dir.join(name);
                    }
                }
            }
            format!("powerpc-eabi-{tool}").into()
        };
        Ok(Toolchain { ar: sibling("ar"), nm: sibling("nm"), gcc })
    }
}

/// devkitPro's folder: DEVKITPRO, or else its usual place (/opt/devkitpro,
/// or on Windows the folder devkitPro's MSYS2 mounts there, usually
/// C:\devkitPro), as a path this program can open, see host_path.
pub fn devkitpro() -> PathBuf {
    match std::env::var_os("DEVKITPRO").filter(|value| !value.is_empty()) {
        Some(value) => host_path(&value.to_string_lossy()),
        None => host_path("/opt/devkitpro"),
    }
}

/// devkitPPC's folder: DEVKITPPC, or else devkitpro()/devkitPPC.
pub fn devkitppc() -> PathBuf {
    match std::env::var_os("DEVKITPPC").filter(|value| !value.is_empty()) {
        Some(value) => host_path(&value.to_string_lossy()),
        None => devkitpro().join("devkitPPC"),
    }
}

/// a path from the environment as this program opens it. elsewhere it is
/// the path as it is, but on Windows devkitPro's installer sets DEVKITPRO
/// to /opt/devkitpro, a path only the programs of its MSYS2 understand.
/// such a path becomes a Windows one (C:/devkitPro/...): what cygpath says
/// when it runs (from MSYS2's shell) and that is there, or else what the
/// mounts in the fstab of devkitPro's or MSYS2's usual installation say,
/// or else C:\devkitPro for /opt/devkitpro.
pub fn host_path(value: &str) -> PathBuf {
    if !cfg!(windows) || !value.starts_with('/') {
        return value.into();
    }
    // another MSYS2's cygpath, Git Bash's say, maps /opt elsewhere
    if let Some(converted) = cygpath(value).filter(|converted| Path::new(converted).exists()) {
        return converted.into();
    }
    let drive = std::env::var("SystemDrive").ok().filter(|drive| drive.len() == 2).unwrap_or_else(|| "C:".to_owned());
    let mut roots: Vec<String> = Vec::new();
    for base in [drive.as_str(), "C:"] {
        for root in [format!("{base}/devkitPro/msys2"), format!("{base}/msys64")] {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    let mut mounts = Vec::new();
    for root in &roots {
        if let Ok(fstab) = std::fs::read_to_string(Path::new(root).join("etc").join("fstab")) {
            mounts.extend(fstab_mounts(&fstab));
        }
    }
    let root = roots.iter().find(|root| Path::new(root).is_dir()).map(String::as_str);
    windows_path(value, &mounts, root, &format!("{drive}/devkitPro")).into()
}

/// what cygpath -m makes of an MSYS2 path, when there is a cygpath to run.
fn cygpath(value: &str) -> Option<String> {
    let output = Command::new("cygpath").arg("-m").arg(value).stderr(std::process::Stdio::null()).output().ok()?;
    let converted = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !converted.is_empty() && !converted.starts_with('/')).then_some(converted)
}

/// the mounts an MSYS2 or Cygwin fstab lists, (Windows folder, mount
/// point), spaces in either written \040.
fn fstab_mounts(fstab: &str) -> Vec<(String, String)> {
    fstab
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(|field| field.replace("\\040", " "));
            let (folder, point) = (fields.next()?, fields.next()?);
            (folder != "none" && point.starts_with('/')).then(|| (folder.replace('\\', "/"), point))
        })
        .collect()
}

/// an MSYS2 path (value starts with /) as a Windows one with forward
/// slashes, the way cygpath -m writes it: /c/x and /cygdrive/c/x are C:/x,
/// a mount's point is its folder (the longest that fits first), and
/// anything else is inside the MSYS2 root, when there is one. /opt/devkitpro
/// with neither is devkitpro, the installer's default.
fn windows_path(value: &str, mounts: &[(String, String)], root: Option<&str>, devkitpro: &str) -> String {
    let under = |path: &str, prefix: &str| -> Option<String> {
        let prefix = prefix.trim_end_matches('/');
        let rest = path.strip_prefix(prefix)?;
        (rest.is_empty() || rest.starts_with('/')).then(|| rest.to_owned())
    };
    let join = |folder: &str, rest: &str| format!("{}{rest}", folder.trim_end_matches('/'));
    let drive = |path: &str| -> Option<String> {
        let mut parts = path.splitn(3, '/').skip(1);
        let letter = parts.next().filter(|part| part.len() == 1 && part.as_bytes()[0].is_ascii_alphabetic())?;
        Some(format!("{}:/{}", letter.to_ascii_uppercase(), parts.next().unwrap_or("")))
    };
    if let Some(path) = value.strip_prefix("/cygdrive").filter(|path| path.starts_with('/')).and_then(drive) {
        return path;
    }
    let mut mounts: Vec<&(String, String)> = mounts.iter().collect();
    mounts.sort_by_key(|(_, point)| std::cmp::Reverse(point.len()));
    if let Some(path) = mounts.iter().find_map(|(folder, point)| under(value, point).map(|rest| join(folder, &rest))) {
        return path;
    }
    if let Some(path) = drive(value) {
        return path;
    }
    if let Some(rest) = under(value, "/opt/devkitpro") {
        // devkitPro's MSYS2 is in its folder, C:/devkitPro/msys2
        let folder = root.and_then(|root| root.strip_suffix("/msys2")).unwrap_or(devkitpro);
        return join(folder, &rest);
    }
    match root {
        Some(root) => join(root, value),
        None => value.to_owned(),
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
    let names: Vec<&Path> = objects.iter().map(|object| object.strip_prefix(code).unwrap_or(object)).collect();
    for batch in batches(&names) {
        let status = Command::new(ar).current_dir(code).arg("rcs").arg(&library).args(batch).status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => return Err(format!("archiving with {} failed, {status}", ar.display())),
            Err(error) => return Err(format!("archiving with {} failed, {error}", ar.display())),
        }
    }
    Ok(())
}

/// names in runs of at most BATCH, BATCH_CHARS long together (a quoted
/// name and a space each), one name to a run when one is longer.
fn batches<'a>(names: &'a [&'a Path]) -> Vec<&'a [&'a Path]> {
    let mut batches = Vec::new();
    let (mut start, mut chars) = (0, 0);
    for (i, name) in names.iter().enumerate() {
        let length = name.as_os_str().len() + 3;
        if i > start && (i - start == BATCH || chars + length > BATCH_CHARS) {
            batches.push(&names[start..i]);
            (start, chars) = (i, 0);
        }
        chars += length;
    }
    if start < names.len() {
        batches.push(&names[start..]);
    }
    batches
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

    /// ar gets the objects in runs short enough for a Windows command line.
    #[test]
    fn objects_are_archived_in_short_runs() {
        let short: Vec<PathBuf> = (0..600).map(|i| PathBuf::from(format!("code{i:03}.o"))).collect();
        let short: Vec<&Path> = short.iter().map(PathBuf::as_path).collect();
        let runs = batches(&short);
        assert_eq!(runs.iter().map(|run| run.len()).collect::<Vec<_>>(), [256, 256, 88]);
        let long: Vec<PathBuf> = (0..300).map(|i| PathBuf::from(format!("hand/{}{i}.o", "x".repeat(200)))).collect();
        let long: Vec<&Path> = long.iter().map(PathBuf::as_path).collect();
        let runs = batches(&long);
        assert!(runs.iter().all(|run| run.iter().map(|name| name.as_os_str().len() + 3).sum::<usize>() <= BATCH_CHARS));
        assert_eq!(runs.iter().map(|run| run.len()).sum::<usize>(), 300);
        assert!(batches(&[]).is_empty());
    }

    /// devkitPro's MSYS2 paths as the Windows ones a native program opens.
    #[test]
    fn msys_paths_become_windows_ones() {
        let fstab = "# comment\nnone / cygdrive binary,posix=0,noacl,user 0 0\n\
                     D:\\dev\\040kits\\devkitPro /opt/devkitpro ntfs binary,posix=0,noacl,user 0 0 # devkitPro\n";
        let mounts = fstab_mounts(fstab);
        assert_eq!(mounts, vec![("D:/dev kits/devkitPro".to_owned(), "/opt/devkitpro".to_owned())]);
        let root = Some("D:/dev kits/devkitPro/msys2");
        assert_eq!(windows_path("/opt/devkitpro", &mounts, root, "C:/devkitPro"), "D:/dev kits/devkitPro");
        assert_eq!(windows_path("/opt/devkitpro/devkitPPC", &mounts, root, "C:/devkitPro"), "D:/dev kits/devkitPro/devkitPPC");
        assert_eq!(windows_path("/opt/devkitprox", &mounts, root, "C:/devkitPro"), "D:/dev kits/devkitPro/msys2/opt/devkitprox");
        assert_eq!(windows_path("/c/Users/me", &mounts, root, "C:/devkitPro"), "C:/Users/me");
        assert_eq!(windows_path("/cygdrive/e/x", &[], None, "C:/devkitPro"), "E:/x");
        assert_eq!(windows_path("/usr/bin", &mounts, root, "C:/devkitPro"), "D:/dev kits/devkitPro/msys2/usr/bin");
        // no fstab: devkitPro's MSYS2 lives in its folder, or the default
        assert_eq!(windows_path("/opt/devkitpro/tools/bin", &[], Some("E:/devkitPro/msys2"), "C:/devkitPro"), "E:/devkitPro/tools/bin");
        assert_eq!(windows_path("/opt/devkitpro", &[], Some("C:/msys64"), "C:/devkitPro"), "C:/devkitPro");
        assert_eq!(windows_path("/opt/devkitpro", &[], None, "C:/devkitPro"), "C:/devkitPro");
        // elsewhere paths stay as they are
        if !cfg!(windows) {
            assert_eq!(host_path("/opt/devkitpro"), PathBuf::from("/opt/devkitpro"));
        }
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
