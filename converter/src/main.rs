//! cia2wiiu: a 3DS game (.cia, .3ds, .cxi) into a Wii U title.
//!
//! 1. decrypts the game into a standalone .cxi, with the user's own keys
//!    when it is encrypted;
//! 2. recompiles its code with 3dsrecomp into C, compiled for the Wii U's
//!    PowerPC;
//! 3. links that with the Zakuro runtime and the Wii U platform layer (the
//!    kit) into an RPX;
//! 4. packs the RPX, the game and its icon as a .wuhb (Aroma), or as the
//!    unpacked code/content/meta layout of an installed title, which --pack
//!    packs into an installable WUP (as NUSPacker would).

mod crtfix;
mod cxi;
mod images;
mod nus;
mod recomp;
mod toolchain;
mod wup;

use std::path::{Path, PathBuf};
use std::time::Instant;

use toolchain::{run, Kit, Toolchain};

const USAGE: &str = "\
usage: cia2wiiu <game.cia|.3ds|.cxi> [options]
       cia2wiiu pack <title folder> -o <wup folder> --common-key <file>
                     [--title-key <hex>] [--cert <title.cert>]
                          packs a title --format wup already wrote (its
                          code/ content/ meta/) into an installable WUP,
                          without converting again

options:
  -o, --output <path>     the .wuhb to write, or the folder for --format wup
                          (default: next to the game, named after it)
  --format <wuhb|wup>     wuhb: one file for Aroma's Homebrew on Menu (default)
                          wup: the unpacked title, code/ content/ meta/, to
                          install once packed (--pack, or NUSPacker)
  --pack                  with --format wup: write an installable WUP
                          (title.tmd, title.tik, title.cert, *.app, *.h3) for
                          WUP Installer GX2 on a console with signature
                          patches (Aroma, Mocha); the unpacked title goes in
                          the work folder (kept with --keep)
  --common-key <file>     for --pack: the Wii U common key, from your own
                          console, as 16 raw bytes or 32 hex digits (or set
                          WIIU_COMMON_KEY_FILE to the file)
  --title-key <hex>       for --pack: the title key, 32 hex digits (default: a
                          random one for each pack)
  --cert <title.cert>     for --pack: a title.cert to copy, e.g. from a title
                          you dumped from your console (default: a stand-in
                          chain with the right layout and no real signatures)
  --layout <split|tv|gamepad>
                          the screens on first start (R3 cycles them in game):
                          split   top screen on the TV, bottom on the GamePad
                          tv      both screens on the TV
                          gamepad both screens on the GamePad
  --renderer <gpu|cpu>    where the 3D is drawn by default: gpu, GX2 on the
                          Wii U's GPU (the default, needs a kit built with
                          CafeGLSL), or cpu, Zakuro's software rasterizer.
                          the player can change it in wiiu.cfg
  --keys <aes_keys.txt>   keys for an encrypted game, from your own console
  --seeddb <seeddb.bin>   seeds for titles that use seed encryption
  --rom-on-sd             leave the game out of the bundle; copy the .cxi it
                          writes to sd:/wiiu/zakuro/<title id>/game.cxi
  --optimize <size|speed> compile the recompiled code for size (-Os, about a
                          quarter smaller, the default) or speed (-O2)
  --interpreter           don't recompile, run everything in the interpreter
  --overrides <file|dir>  hand-written C replacing recompiled functions
  --kit <dir>             the prebuilt Zakuro Wii U kit (default: ./kit)
  --name <text>           the name shown on the menu
  --work <dir>            where intermediate files go (default: next to output)
  --keep                  keep the intermediate files
  --allow-large           write a .wuhb even when its code is past what Aroma
                          loads (about 56 MiB); it will fail on the console
  --new3ds                run as a New 3DS (titles that need it get it anyway)
";

struct Options {
    rom: PathBuf,
    output: Option<PathBuf>,
    wup: bool,
    pack: bool,
    common_key: Option<PathBuf>,
    title_key: Option<String>,
    cert: Option<PathBuf>,
    layout: i32,
    renderer: u8,
    keys: Option<PathBuf>,
    seeddb: Option<PathBuf>,
    rom_on_sd: bool,
    interpreter: bool,
    overrides: Option<PathBuf>,
    kit: Option<PathBuf>,
    name: Option<String>,
    work: Option<PathBuf>,
    keep: bool,
    new3ds: bool,
    optimize_size: bool,
    allow_large: bool,
}

fn parse() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut options = Options {
        rom: PathBuf::new(),
        output: None,
        wup: false,
        pack: false,
        common_key: None,
        title_key: None,
        cert: None,
        layout: 0,
        renderer: 1,
        keys: None,
        seeddb: None,
        rom_on_sd: false,
        interpreter: false,
        overrides: None,
        kit: None,
        name: None,
        work: None,
        keep: false,
        new3ds: false,
        optimize_size: true,
        allow_large: false,
    };
    let mut rom = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-o" | "--output" => options.output = Some(value(&arg)?.into()),
            "--format" => {
                options.wup = match value(&arg)?.as_str() {
                    "wuhb" => false,
                    "wup" => true,
                    other => return Err(format!("unknown format {other}, wuhb or wup")),
                }
            }
            "--pack" => options.pack = true,
            "--common-key" => options.common_key = Some(value(&arg)?.into()),
            "--title-key" => options.title_key = Some(value(&arg)?),
            "--cert" => options.cert = Some(value(&arg)?.into()),
            "--layout" => {
                options.layout = match value(&arg)?.as_str() {
                    "split" | "0" => 0,
                    "tv" | "1" => 1,
                    "gamepad" | "drc" | "2" => 2,
                    other => return Err(format!("unknown layout {other}, split, tv or gamepad")),
                }
            }
            "--renderer" => {
                options.renderer = match value(&arg)?.as_str() {
                    "gpu" | "gx2" | "1" => 1,
                    "cpu" | "software" | "0" => 0,
                    other => return Err(format!("unknown renderer {other}, gpu or cpu")),
                }
            }
            "--optimize" => {
                options.optimize_size = match value(&arg)?.as_str() {
                    "size" | "s" => true,
                    "speed" | "2" => false,
                    other => return Err(format!("unknown optimization {other}, size or speed")),
                }
            }
            "--keys" => options.keys = Some(value(&arg)?.into()),
            "--seeddb" => options.seeddb = Some(value(&arg)?.into()),
            "--rom-on-sd" => options.rom_on_sd = true,
            "--interpreter" => options.interpreter = true,
            "--overrides" => options.overrides = Some(value(&arg)?.into()),
            "--kit" => options.kit = Some(value(&arg)?.into()),
            "--name" => options.name = Some(value(&arg)?),
            "--work" => options.work = Some(value(&arg)?.into()),
            "--keep" => options.keep = true,
            "--new3ds" => options.new3ds = true,
            "--allow-large" => options.allow_large = true,
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n\n{USAGE}")),
            _ if rom.is_none() => rom = Some(PathBuf::from(arg)),
            _ => return Err(format!("one game at a time; {arg} is extra")),
        }
    }
    options.rom = rom.ok_or_else(|| USAGE.to_owned())?;
    if options.pack {
        if !options.wup {
            return Err("--pack packs an installable title, so it goes with --format wup".into());
        }
        if options.common_key.is_none() {
            options.common_key = std::env::var_os("WIIU_COMMON_KEY_FILE").filter(|path| !path.is_empty()).map(PathBuf::from);
        }
        if options.common_key.is_none() {
            return Err("--pack needs the Wii U common key, which you supply yourself: --common-key <file>, or WIIU_COMMON_KEY_FILE=<file>".into());
        }
    } else if options.common_key.is_some() || options.title_key.is_some() || options.cert.is_some() {
        return Err("--common-key, --title-key and --cert are for --pack".into());
    }
    Ok(options)
}

fn step(text: &str) {
    println!("==> {text}");
}

/// a C string literal for text.
fn c_string(text: &str) -> String {
    let mut out = String::from("\"");
    for byte in text.bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7E => out.push(byte as char),
            _ => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out.push('"');
    out
}

/// a name safe for files.
fn file_name(text: &str) -> String {
    let name: String = text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let name = name.trim_matches('_').to_owned();
    if name.is_empty() { "game".into() } else { name }
}

fn convert(options: &Options) -> Result<(), String> {
    let started = Instant::now();
    let toolchain = Toolchain::find()?;
    let kit = Kit::find(options.kit.as_deref())?;
    if !options.rom.is_file() {
        return Err(format!("{} is not a file", options.rom.display()));
    }
    // the keys are checked now rather than after the slow steps
    let pack_keys = match &options.common_key {
        Some(common_key) if options.pack => Some(nus::PackKeys::load(common_key, options.title_key.as_deref(), options.cert.as_deref())?),
        _ => None,
    };

    let stem = options.rom.file_stem().map(|s| file_name(&s.to_string_lossy())).unwrap_or_else(|| "game".into());
    let output = match &options.output {
        Some(output) => output.clone(),
        None => {
            let dir = options.rom.parent().unwrap_or(Path::new("."));
            if options.wup { dir.join(format!("{stem}_wup")) } else { dir.join(format!("{stem}.wuhb")) }
        }
    };
    let work = match &options.work {
        Some(work) => work.clone(),
        None => output.with_file_name(format!("{stem}.cia2wiiu-work")),
    };
    let fresh = !work.exists();
    std::fs::create_dir_all(&work).map_err(|e| format!("could not create {}, {e}", work.display()))?;
    // a work folder this run made goes again when the run fails
    let guard = WorkGuard { dir: fresh.then(|| work.clone()) };
    let content = work.join("content");
    std::fs::create_dir_all(&content).map_err(|e| format!("could not create {}, {e}", content.display()))?;

    // 1. the game, decrypted, as a .cxi
    step(&format!("reading {}", options.rom.display()));
    let cxi_path = content.join("game.cxi");
    let decrypted = recomp::decrypt(&options.rom, &cxi_path, options.keys.as_deref(), options.seeddb.as_deref())?;
    println!("    {} ({:016X}){}", decrypted.product_code, decrypted.title_id, if decrypted.was_encrypted { ", decrypted" } else { "" });
    let cxi = cxi::read(&cxi_path)?;
    let smdh = cxi.icon.as_deref().and_then(images::Smdh::parse);
    let long_name = options
        .name
        .clone()
        .or_else(|| smdh.as_ref().map(|s| if s.long_name.is_empty() { s.short_name.clone() } else { s.long_name.clone() }))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| stem.clone());
    let short_name = options.name.clone().or_else(|| smdh.as_ref().map(|s| s.short_name.clone())).filter(|n| !n.is_empty()).unwrap_or_else(|| long_name.clone());
    let publisher = smdh.as_ref().map(|s| s.publisher.clone()).filter(|p| !p.is_empty()).unwrap_or_else(|| "Unknown".into());
    println!("    {short_name} by {publisher}");

    // 2. the code, recompiled to C and compiled for the Wii U
    let code_dir = work.join("code");
    let code = if options.interpreter {
        step("skipping recompilation, the game runs in the interpreter");
        None
    } else {
        step("recompiling the game's code (this takes a while)");
        let info = recomp::recompile(&cxi_path, &code_dir, options.overrides.as_deref(), options.optimize_size)?;
        println!("    {}", info.summary);
        Some(info)
    };

    // 3. linking: the game's info, its code, the runtime and the platform
    step("linking the Wii U program");
    let template = std::fs::read_to_string(kit.game_info_template()).map_err(|e| format!("could not read the kit's game_info template, {e}"))?;
    let game_info = template
        .replace("@PROGRAM_ID@", &format!("0x{:016X}", cxi.program_id))
        .replace("@TITLE@", &c_string(&short_name))
        .replace("@LAYOUT@", &options.layout.to_string())
        .replace("@ROM_NAME@", &c_string("game.cxi"))
        .replace("@RENDERER@", &options.renderer.to_string())
        .replace("@NEW3DS@", if options.new3ds || cxi.new3ds_only { "1" } else { "0" })
        .replace("@HAS_CODE@", if code.is_some() { "1" } else { "0" });
    let game_info_c = work.join("game_info.c");
    std::fs::write(&game_info_c, game_info).map_err(|e| format!("could not write {}, {e}", game_info_c.display()))?;
    let game_info_o = work.join("game_info.o");
    run(
        toolchain.command(&toolchain.gcc).args(toolchain.cflags()).arg(format!("-I{}", kit.include().display())).arg("-c").arg(&game_info_c).arg("-o").arg(&game_info_o),
        "compiling game_info.c",
    )?;

    let elf = work.join("game.elf");
    let mut link = toolchain.command(&toolchain.gcc);
    // -Xlinker and not -Wl, which would split a path with a comma in it
    link.arg(format!("-specs={}", toolchain.wut.join("share").join("wut.specs").display()))
        .args(["-mcpu=750", "-meabi", "-mhard-float"])
        .args(["-Xlinker", "-Map", "-Xlinker"])
        .arg(work.join("game.map"))
        .args(kit.objects())
        .arg(&game_info_o)
        .arg("-Wl,--start-group");
    if let Some(code) = &code {
        link.arg(&code.library);
    }
    link.arg(kit.runtime())
        .arg(format!("-L{}", toolchain.portlibs.join("lib").display()))
        .arg(format!("-L{}", toolchain.wut.join("lib").display()))
        .args(["-lSDL2_test", "-lSDL2", "-lwut", "-lm"])
        .arg("-Wl,--end-group")
        .arg("-o")
        .arg(&elf);
    run(&mut link, "linking")?;
    // past 32 MiB of code the linker puts long-branch stubs inside __init
    // and __fini, which then loop forever (see crtfix.rs)
    let fixed = crtfix::patch_elf(&elf)?;
    if fixed > 0 {
        println!("    repaired {fixed} long-branch stub{} in the startup code", if fixed == 1 { "" } else { "s" });
    }

    // the Rust objects carry a .note.GNU-stack section elf2rpl can't place
    let stripped = work.join("game.stripped.elf");
    run(toolchain.command(&toolchain.objcopy).args(["-R", ".note.GNU-stack", "-R", ".comment"]).arg(&elf).arg(&stripped), "objcopy")?;
    let rpx = work.join(format!("{}.rpx", file_name(&short_name)));
    run(toolchain.command(&toolchain.elf2rpl).arg(&stripped).arg(&rpx), "elf2rpl")?;
    let code_size = text_size(&toolchain, &stripped).unwrap_or(0);
    println!("    {} ({:.1} MiB, {:.1} MiB of code)", rpx.display(), file_mib(&rpx), code_size as f64 / (1 << 20) as f64);
    check_code_size(code_size, options)?;

    // the game on the SD card instead of in the bundle
    if options.rom_on_sd {
        let kept = output.with_file_name(format!("{:016X}-game.cxi", cxi.program_id));
        if !options.wup {
            std::fs::rename(&cxi_path, &kept).map_err(|e| format!("could not move the .cxi to {}, {e}", kept.display()))?;
            println!("    copy {} to sd:/wiiu/zakuro/{:016X}/game.cxi", kept.display(), cxi.program_id);
        }
    }

    // 4. the package
    let icon = smdh.as_ref().map(|s| &s.icon);
    if options.wup {
        // packed, the WUP goes to the output and the title it is packed from
        // to the work folder
        let layout = if pack_keys.is_some() { work.join("title") } else { output.clone() };
        if pack_keys.is_some() {
            // everything in it is packed, so nothing of an earlier run may stay
            remove_dir(&layout).map_err(|e| format!("could not clear {}, {e}", layout.display()))?;
        }
        step("writing the installable title layout");
        let title_id = wup::title_id_for(cxi.program_id);
        let rpx_name = rpx.file_name().unwrap().to_string_lossy().into_owned();
        wup::write_layout(
            &layout,
            &wup::WupInfo {
                title_id,
                rpx_name: &rpx_name,
                long_name: &long_name,
                short_name: &short_name,
                publisher: &publisher,
                product_code: &decrypted.product_code,
                code_size,
            },
            icon,
        )?;
        move_file(&rpx, &layout.join("code").join(&rpx_name))?;
        if cxi_path.exists() {
            move_file(&cxi_path, &layout.join("content").join("game.cxi"))?;
        }
        println!("    {} (Wii U title {:016X})", layout.display(), title_id);
        match &pack_keys {
            Some(keys) => {
                step(&format!("packing the WUP ({})", if keys.random_title_key { "with a random title key" } else { "with the given title key" }));
                let packed = nus::pack(&layout, &output, keys, &mut |line| println!("    {line}"))?;
                println!(
                    "    title.tmd, title.tik, title.cert{}",
                    if keys.cert.is_some() { " (copied from --cert)" } else { " (a stand-in chain; see --cert if an installer rejects it)" }
                );
                println!(
                    "    {} (Wii U title {:016X}, {} contents, {:.1} MiB)",
                    output.display(),
                    packed.title_id,
                    packed.contents,
                    packed.bytes as f64 / (1 << 20) as f64
                );
                println!("    copy the folder to sd:/install/ and install it with WUP Installer GX2 (needs signature patches: Aroma or Mocha)");
            }
            None => println!("    pack it: cia2wiiu pack {} -o <wup folder> --common-key <file>, then install the WUP", layout.display()),
        }
    } else {
        step("packing the .wuhb");
        let pictures = work.join("pictures");
        std::fs::create_dir_all(&pictures).map_err(|e| format!("could not create {}, {e}", pictures.display()))?;
        let (icon_tga, tv_tga, drc_tga) = (pictures.join("icon.tga"), pictures.join("tv.tga"), pictures.join("drc.tga"));
        images::menu_icon(icon).write_tga(&icon_tga, true)?;
        images::splash(icon, 1280, 720).write_tga(&tv_tga, false)?;
        images::splash(icon, 854, 480).write_tga(&drc_tga, false)?;
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("could not create {}, {e}", parent.display()))?;
        }
        let mut pack = toolchain.command(&toolchain.wuhbtool);
        pack.arg(&rpx).arg(&output);
        // wuhbtool wants a content folder; an empty one when the game is on the SD card
        pack.arg(format!("--content={}", content.display()));
        pack.arg(format!("--name={long_name}"))
            .arg(format!("--short-name={short_name}"))
            .arg(format!("--author={publisher}"))
            .arg(format!("--icon={}", icon_tga.display()))
            .arg(format!("--tv-image={}", tv_tga.display()))
            .arg(format!("--drc-image={}", drc_tga.display()));
        run(&mut pack, "wuhbtool")?;
        println!("    {} ({:.1} MiB)", output.display(), file_mib(&output));
        println!("    put it in sd:/wiiu/apps/ and start it from the Wii U menu (Aroma)");
    }

    std::mem::forget(guard);
    if !options.keep {
        // the output is written by now, so a folder that won't go is only
        // worth a warning
        if let Err(e) = remove_dir(&work) {
            println!("    warning: could not clean up {}, {e}", work.display());
        }
    } else {
        println!("    intermediate files kept in {}", work.display());
        if pack_keys.is_some() {
            println!("    the unpacked title is in {}", work.join("title").display());
        }
    }
    println!("done in {:.0?}", started.elapsed());
    Ok(())
}

struct WorkGuard {
    dir: Option<PathBuf>,
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = remove_dir(dir);
        }
    }
}

/// removes a folder and what is in it, trying again for a while: on Windows
/// a virus scanner or the indexer may still have a file just written open.
fn remove_dir(dir: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::remove_dir_all(dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound && tries < 10 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            result => return result,
        }
    }
}

fn file_mib(path: &Path) -> f64 {
    std::fs::metadata(path).map_or(0.0, |m| m.len() as f64 / (1 << 20) as f64)
}

fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map_err(|e| format!("could not copy {} to {}, {e}", from.display(), to.display()))?;
    std::fs::remove_file(from).map_err(|e| format!("could not remove {}, {e}", from.display()))
}

/// Aroma starts a .wuhb in place of Health & Safety with a code heap Mocha
/// sets to 64 MiB whatever the bundle says; the system's own non-shared
/// libraries (sndcore2, proc_ui, nn_*) and the loader's tracking come out of
/// it too, so this much is what a .wuhb's own code can use.
const WUHB_CODE_LIMIT: u64 = 56 << 20;
/// the most code any title's cos.xml may ask for, 0x0E000000, less room for
/// the system libraries.
const WUP_CODE_LIMIT: u64 = 0x0E00_0000 - (16 << 20);

fn check_code_size(code: u64, options: &Options) -> Result<(), String> {
    let mib = |bytes: u64| bytes as f64 / (1 << 20) as f64;
    if code > WUP_CODE_LIMIT {
        return Err(format!(
            "the program has {:.1} MiB of code, more than any Wii U title can load ({:.0} MiB); \
             try --optimize size, or --interpreter",
            mib(code),
            mib(WUP_CODE_LIMIT)
        ));
    }
    if !options.wup && code > WUHB_CODE_LIMIT {
        let message = format!(
            "the program has {:.1} MiB of code, but Aroma loads a .wuhb with a 64 MiB code area (about {:.0} MiB of it for the \
             program), so it would fail on the console with \"Could not allocate uncompressed text\" (Cemu doesn't enforce \
             this, so it would seem to work there). make an installed title instead with --format wup, whose cos.xml asks \
             for the room it needs{}",
            mib(code),
            mib(WUHB_CODE_LIMIT),
            if options.optimize_size { "" } else { ", or try --optimize size" }
        );
        if options.allow_large {
            println!("    warning: {message}");
        } else {
            return Err(format!("{message}. --allow-large writes the .wuhb anyway."));
        }
    }
    Ok(())
}

/// the size of the program's .text, from objdump's headers.
fn text_size(toolchain: &Toolchain, elf: &Path) -> Option<u64> {
    let output = toolchain.command(&toolchain.objdump).arg("-h").arg(elf).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let mut total = 0u64;
    for pair in lines.windows(2) {
        let fields: Vec<&str> = pair[0].split_whitespace().collect();
        if fields.len() >= 3 && fields[1] == ".text" && pair[1].contains("CODE") {
            total += u64::from_str_radix(fields[2], 16).unwrap_or(0);
        }
    }
    Some(total)
}

/// cia2wiiu pack <folder> -o <out> --common-key <file> [--title-key] [--cert]
fn pack_only(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut layout = None;
    let mut output = None;
    let mut common_key = std::env::var_os("WIIU_COMMON_KEY_FILE").map(PathBuf::from);
    let mut title_key = None;
    let mut cert = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-o" | "--output" => output = Some(PathBuf::from(value(&arg)?)),
            "--common-key" => common_key = Some(PathBuf::from(value(&arg)?)),
            "--title-key" => title_key = Some(value(&arg)?),
            "--cert" => cert = Some(PathBuf::from(value(&arg)?)),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other} for pack\n\n{USAGE}")),
            other if layout.is_none() => layout = Some(PathBuf::from(other)),
            other => return Err(format!("pack takes one title folder, {other} is a second")),
        }
    }
    let layout = layout.ok_or_else(|| format!("pack needs the title folder --format wup wrote\n\n{USAGE}"))?;
    let output = output.ok_or("pack needs -o <folder> for the WUP")?;
    let common_key = common_key.ok_or("pack needs --common-key <file> (or WIIU_COMMON_KEY_FILE), the Wii U common key from your own console")?;
    if output.exists() && std::fs::read_dir(&output).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(format!("{} exists and isn't empty; pick a new folder for the WUP", output.display()));
    }
    let keys = nus::PackKeys::load(&common_key, title_key.as_deref(), cert.as_deref())?;
    let started = Instant::now();
    step(&format!("packing {} ({})", layout.display(), if keys.random_title_key { "with a random title key" } else { "with the given title key" }));
    let packed = nus::pack(&layout, &output, &keys, &mut |line| println!("    {line}"))?;
    println!(
        "    title.tmd, title.tik, title.cert{}",
        if keys.cert.is_some() { " (copied from --cert)" } else { " (a stand-in chain; see --cert if an installer rejects it)" }
    );
    println!(
        "    {} (Wii U title {:016X}, {} contents, {:.1} MiB)",
        output.display(),
        packed.title_id,
        packed.contents,
        packed.bytes as f64 / (1024.0 * 1024.0)
    );
    println!("    install it with WUP Installer GX2 (or a similar installer) on a console with signature patches");
    println!("done in {}s", started.elapsed().as_secs());
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) == Some("pack") {
        args.next();
        if let Err(message) = pack_only(args) {
            eprintln!("cia2wiiu: {message}");
            std::process::exit(1);
        }
        return;
    }
    let options = match parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("cia2wiiu: {message}");
            std::process::exit(2);
        }
    };
    if let Err(message) = convert(&options) {
        eprintln!("cia2wiiu: {message}");
        std::process::exit(1);
    }
}
