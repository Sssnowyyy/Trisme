//! devkitPro's tools (devkitPPC, wut, SDL2 for the Wii U, elf2rpl,
//! wuhbtool) and the prebuilt Zakuro kit the game links against.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Toolchain {
    pub gcc: PathBuf,
    pub objcopy: PathBuf,
    pub elf2rpl: PathBuf,
    pub wuhbtool: PathBuf,
    pub wut: PathBuf,
    pub portlibs: PathBuf,
}

/// the prebuilt pieces every game links: the runtime and the platform layer.
pub struct Kit {
    pub dir: PathBuf,
}

impl Kit {
    pub fn runtime(&self) -> PathBuf {
        self.dir.join("lib/libzakuro_wiiu.a")
    }

    pub fn objects(&self) -> Vec<PathBuf> {
        ["main.o", "layout.o", "newlib_shims.o"].iter().map(|name| self.dir.join("obj").join(name)).collect()
    }

    pub fn include(&self) -> PathBuf {
        self.dir.join("include")
    }

    pub fn game_info_template(&self) -> PathBuf {
        self.dir.join("share/game_info_template.c")
    }

    /// finds the kit: the given folder, $ZAKURO_WIIU_KIT, or kit/ beside or
    /// above the program.
    pub fn find(given: Option<&Path>) -> Result<Kit, String> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(given) = given {
            candidates.push(given.to_owned());
        }
        if let Some(dir) = std::env::var_os("ZAKURO_WIIU_KIT") {
            candidates.push(dir.into());
        }
        if let Ok(exe) = std::env::current_exe() {
            for ancestor in exe.ancestors().skip(1).take(5) {
                candidates.push(ancestor.join("kit"));
            }
        }
        candidates.push(PathBuf::from("kit"));
        for dir in &candidates {
            let kit = Kit { dir: dir.clone() };
            if kit.runtime().is_file() && kit.objects().iter().all(|object| object.is_file()) {
                return Ok(kit);
            }
        }
        Err(format!(
            "could not find the Zakuro Wii U kit (lib/libzakuro_wiiu.a and obj/*.o); build it with tools/build-kit.sh, or pass --kit. looked in: {}",
            candidates.iter().map(|dir| dir.display().to_string()).collect::<Vec<_>>().join(", ")
        ))
    }
}

impl Toolchain {
    pub fn find() -> Result<Toolchain, String> {
        let devkitpro = std::env::var_os("DEVKITPRO").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/opt/devkitpro"));
        let devkitppc = std::env::var_os("DEVKITPPC").map(PathBuf::from).unwrap_or_else(|| devkitpro.join("devkitPPC"));
        let exe = if cfg!(windows) { ".exe" } else { "" };
        let toolchain = Toolchain {
            gcc: devkitppc.join(format!("bin/powerpc-eabi-gcc{exe}")),
            objcopy: devkitppc.join(format!("bin/powerpc-eabi-objcopy{exe}")),
            elf2rpl: devkitpro.join(format!("tools/bin/elf2rpl{exe}")),
            wuhbtool: devkitpro.join(format!("tools/bin/wuhbtool{exe}")),
            wut: devkitpro.join("wut"),
            portlibs: devkitpro.join("portlibs/wiiu"),
        };
        let needed = [
            (&toolchain.gcc, "devkitPPC (pacman -S devkitPPC)"),
            (&toolchain.objcopy, "devkitPPC (pacman -S devkitPPC)"),
            (&toolchain.elf2rpl, "wut-tools (pacman -S wut-tools)"),
            (&toolchain.wuhbtool, "wut-tools (pacman -S wut-tools)"),
        ];
        for (path, package) in needed {
            if !path.is_file() {
                return Err(format!("could not find {}, which comes with {package}; set DEVKITPRO if it is elsewhere", path.display()));
            }
        }
        if !toolchain.wut.join("share/wut.specs").is_file() {
            return Err(format!("could not find wut in {} (pacman -S wut)", toolchain.wut.display()));
        }
        if !toolchain.portlibs.join("lib/libSDL2.a").is_file() {
            return Err(format!("could not find SDL2 for the Wii U in {} (pacman -S wiiu-sdl2)", toolchain.portlibs.display()));
        }
        Ok(toolchain)
    }

    pub fn cflags(&self) -> Vec<String> {
        let mut flags: Vec<String> =
            ["-O2", "-mcpu=750", "-meabi", "-mhard-float", "-DESPRESSO", "-D__WIIU__", "-D__WUT__"].iter().map(|s| s.to_string()).collect();
        flags.push(format!("-I{}", self.wut.join("include").display()));
        flags
    }
}

/// runs a command, its output shown only when it fails.
pub fn run(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command.output().map_err(|e| format!("could not run {what} ({:?}), {e}", command.get_program()))?;
    if !output.status.success() {
        let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        // the last lines say why
        let lines: Vec<&str> = text.lines().collect();
        let tail = lines[lines.len().saturating_sub(40)..].join("\n");
        return Err(format!("{what} failed ({}):\n{tail}", output.status));
    }
    Ok(())
}
