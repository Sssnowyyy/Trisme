//! devkitPro's tools (devkitPPC, wut, SDL2 for the Wii U, elf2rpl,
//! wuhbtool) and the prebuilt Zakuro kit the game links against.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Toolchain {
    /// devkitPro's and devkitPPC's folders, as this program opens them
    pub devkitpro: PathBuf,
    pub devkitppc: PathBuf,
    pub gcc: PathBuf,
    pub objcopy: PathBuf,
    pub objdump: PathBuf,
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
        self.dir.join("lib").join("libzakuro_wiiu.a")
    }

    pub fn objects(&self) -> Vec<PathBuf> {
        ["main.o", "layout.o", "newlib_shims.o", "gx2.o", "gx2_shaders.o"].iter().map(|name| self.dir.join("obj").join(name)).collect()
    }

    pub fn include(&self) -> PathBuf {
        self.dir.join("include")
    }

    pub fn game_info_template(&self) -> PathBuf {
        self.dir.join("share").join("game_info_template.c")
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
    /// devkitPro from DEVKITPRO (and DEVKITPPC), or its usual place. on
    /// Windows the installer sets DEVKITPRO to its MSYS2 path,
    /// /opt/devkitpro, which recomp3ds::wiiu::devkitpro turns into the
    /// Windows folder (C:\devkitPro), whether this runs from MSYS2's shell
    /// or from cmd or PowerShell.
    pub fn find() -> Result<Toolchain, String> {
        let devkitpro = recomp3ds::wiiu::devkitpro();
        let devkitppc = recomp3ds::wiiu::devkitppc();
        let tool = |dir: &Path, name: &str| dir.join("bin").join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let toolchain = Toolchain {
            gcc: tool(&devkitppc, "powerpc-eabi-gcc"),
            objcopy: tool(&devkitppc, "powerpc-eabi-objcopy"),
            objdump: tool(&devkitppc, "powerpc-eabi-objdump"),
            elf2rpl: tool(&devkitpro.join("tools"), "elf2rpl"),
            wuhbtool: tool(&devkitpro.join("tools"), "wuhbtool"),
            wut: devkitpro.join("wut"),
            portlibs: devkitpro.join("portlibs").join("wiiu"),
            devkitpro,
            devkitppc,
        };
        // devkitPro's pacman is pacman in its MSYS2 on Windows
        let pacman = if cfg!(windows) { "pacman" } else { "dkp-pacman" };
        let needed = [
            (&toolchain.gcc, "devkitPPC"),
            (&toolchain.objcopy, "devkitPPC"),
            (&toolchain.elf2rpl, "wut-tools"),
            (&toolchain.wuhbtool, "wut-tools"),
        ];
        for (path, package) in needed {
            if !path.is_file() {
                return Err(format!(
                    "could not find {}, which comes with {package} ({pacman} -S {package}); set DEVKITPRO if devkitPro is elsewhere{}",
                    path.display(),
                    if cfg!(windows) { " (C:\\devkitPro, say)" } else { "" }
                ));
            }
        }
        if !toolchain.wut.join("share").join("wut.specs").is_file() {
            return Err(format!("could not find wut in {} ({pacman} -S wut)", toolchain.wut.display()));
        }
        if !toolchain.portlibs.join("lib").join("libSDL2.a").is_file() {
            return Err(format!("could not find SDL2 for the Wii U in {} ({pacman} -S wiiu-sdl2)", toolchain.portlibs.display()));
        }
        Ok(toolchain)
    }

    /// a command running one of devkitPro's programs, with DEVKITPRO and
    /// DEVKITPPC set to the folders found: wut.specs finds wut.ld through
    /// DEVKITPRO, and the MSYS2 path the Windows installer sets would not
    /// do for gcc run from cmd or PowerShell.
    pub fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command.env("DEVKITPRO", &self.devkitpro).env("DEVKITPPC", &self.devkitppc);
        command
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
