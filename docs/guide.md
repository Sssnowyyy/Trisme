# Zakuro for Wii U

This turns a 3DS game you own (`.cia`, `.3ds` or `.cxi`) into a Wii U title. The output is either a `.wuhb` you start from the Wii U menu under Aroma, or a title you install to the Wii U menu: an installable WUP that the converter packs itself (`--pack`), or the unpacked title folder for NUSPacker.

Two existing projects do the heavy lifting:

- [3dsrecomp](https://github.com/fearkov/3dsrecomp) statically recompiles the game's ARM code into C. Here that C is cross-compiled for the Wii U's PowerPC (Espresso).
- [Zakuro](https://github.com/fearkov/zakuro) is a Rust 3DS emulator that provides the runtime: kernel, services, GPU and sound. It is linked into the same RPX and runs on Rust's standard library through devkitPPC's newlib.

A small C layer built on SDL2 and wut handles everything Wii U-specific: the TV and the GamePad, sound, controllers and the HOME menu.

## Screen layouts

Click the right stick (**R3**) on the GamePad or a Pro Controller to cycle through three layouts. The game remembers your choice in `sd:/wiiu/zakuro/<title id>/wiiu.cfg`.

| Layout | TV | GamePad | Touch |
|---|---|---|---|
| `split` (default) | top screen | bottom screen | touch the bottom screen on the GamePad |
| `tv` | both screens, stacked | blank | the whole GamePad panel acts as the bottom screen |
| `gamepad` | blank | both screens, stacked, at 1:1 pixels | touch the bottom screen on the GamePad |

`--layout` sets which layout a game starts in the first time it runs.

## Controls

| 3DS | GamePad / Pro Controller / Classic Controller |
|---|---|
| A B X Y, L R, ZL ZR | same buttons |
| Start / Select | + / − |
| D-pad | D-pad |
| Circle Pad | left stick |
| C-stick (New 3DS) | right stick |
| Touch screen | GamePad touch screen |
| HOME menu | HOME button |
| Change layout | R3 (right stick click) |

## What you need

- devkitPro with `devkitPPC`, `wut`, `wut-tools` and `wiiu-sdl2` (the `wiiu-dev` group has all but SDL2). For the homebrew test ROM you also need `devkitARM`/`libctru` (the `3ds-dev` group) and `makerom` from [Project_CTR](https://github.com/3DSGuy/Project_CTR/releases).
- Rust, through [rustup](https://rustup.rs), with a nightly toolchain and `rust-src`. The Wii U target builds `std` with `-Z build-std`. A distribution's own `cargo` package won't do, because `tools/build-kit.sh` runs `cargo +nightly`. The converter itself builds with stable Rust 1.88 or newer.
- Your own console's keys, if the dump is encrypted, which retail `.cia` dumps (`*.legit.cia`) are. Put a Citra/Azahar-format `aes_keys.txt` next to the game or in Azahar's `sysdata` folder, or pass `--keys`. You can dump the keys from your own 3DS with GodMode9. Nothing here includes keys. Seed-encrypted titles also need `seeddb.bin`.

### Linux (x86_64 or aarch64)

1. Install devkitPro's pacman as described in [devkitPro's guide](https://devkitpro.org/wiki/devkitPro_pacman). It installs to `/opt/devkitpro`. Then install the packages:
   ```
   sudo dkp-pacman -S wiiu-dev wiiu-sdl2
   sudo dkp-pacman -S 3ds-dev          # only for the homebrew test ROM
   ```
   On Arch and its derivatives the command is `pacman` and the packages come from devkitPro's repositories. Log in again, or `source /etc/profile.d/devkit-env.sh`, so that `DEVKITPRO` is set.
2. You also need a C compiler for the host (`build-essential`, `base-devel` or similar), which Rust uses to link the converter.
3. Install Rust:
   ```
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   rustup toolchain install nightly --profile minimal -c rust-src
   ```

### Windows

1. Run the [devkitPro installer](https://github.com/devkitPro/installer/releases) and pick the Wii U packages. It installs to `C:\devkitPro`, sets `DEVKITPRO=/opt/devkitpro` and comes with its own MSYS2. Open **devkitPro → MSYS2** from the Start menu and install the rest with pacman:
   ```
   pacman -S wiiu-dev wiiu-sdl2
   pacman -S 3ds-dev                   # only for the homebrew test ROM
   ```
2. Install Rust with [rustup-init.exe](https://rustup.rs), then in a new terminal:
   ```
   rustup toolchain install nightly --profile minimal -c rust-src
   ```
   rustup's default `x86_64-pc-windows-msvc` toolchain needs the Visual Studio Build Tools (the "Desktop development with C++" workload) to link the converter and the build scripts. If you don't want those, use the GNU toolchain instead, which brings its own linker: `rustup set default-host x86_64-pc-windows-gnu`, then reinstall `stable` and `nightly`.
3. Run the shell scripts in devkitPro's MSYS2 shell. The scripts find rustup's `cargo` in `%USERPROFILE%\.cargo\bin` even though that MSYS2 leaves the Windows `PATH` out, and they hand native Windows paths (`C:/...`) to cargo and gcc. Another MSYS2 or Git Bash works too if you set `DEVKITPRO=C:/devkitPro`.
4. If git turns line endings into CRLF on checkout (`core.autocrlf=true`), bash refuses the scripts. Clone with `git clone -c core.autocrlf=input ...`.

The converter is a normal Windows program. You can run it from MSYS2, cmd or PowerShell. It turns the MSYS2 path in `DEVKITPRO` into the real folder, `C:\devkitPro` by default, and passes that folder on to gcc. If devkitPro is somewhere else, set `DEVKITPRO` to its Windows path, for example `$env:DEVKITPRO = "D:\devkitPro"`.

### macOS

1. Install devkitPro's pacman with the `.pkg` from [devkitPro's guide](https://devkitpro.org/wiki/devkitPro_pacman), then:
   ```
   sudo dkp-pacman -S wiiu-dev wiiu-sdl2
   sudo dkp-pacman -S 3ds-dev          # only for the homebrew test ROM
   ```
2. Install the Xcode command line tools (`xcode-select --install`) and Rust, either with the rustup installer from [rustup.rs](https://rustup.rs) or with `brew install rustup` (`tools/build-kit.sh` finds Homebrew's keg-only rustup by itself). Then:
   ```
   rustup toolchain install nightly --profile minimal -c rust-src
   ```

## Building

The same commands work on all three systems. On Windows, run them in devkitPro's MSYS2 shell.

```
tools/build-kit.sh                     # the runtime and platform layer for the Wii U -> kit/
cd converter && cargo build --release  # the converter, for your computer
```

For the GPU renderer, the kit also needs [CafeGLSL](https://github.com/Exzap/CafeGLSL/releases)'s offline shader compiler: take v1.0.0-rc1 or newer and the archive for your computer, e.g. `cafeglsl-1.0.0-rc1-macos-arm64.tar.xz` (not the `wiiu-ppc` one; v1.0.0-rc1 is a pre-release, so GitHub's "Latest" label points at v0.2.0, which has no macOS or Windows compiler) and pass its folder, `CAFEGLSL_DIR=/path/to/cafeglsl-... tools/build-kit.sh`. Without it the kit still builds, and games draw their 3D on the CPU. See [docs/gx2.md](docs/gx2.md).

`tools/build-kit.sh` looks for devkitPro in `DEVKITPRO` (default `/opt/devkitpro`). `ZAKURO_WIIU_RUST` picks a toolchain other than `nightly`, `KIT_DIR` sets another output folder and `CARGO_TARGET_DIR` sets another build folder.

`testdata/hello-world/build.sh` rebuilds the homebrew test ROM, `hello.cxi`.

## Converting a game

```
converter/target/release/cia2wiiu "My Game.cia"                        # -> My_Game.wuhb
converter/target/release/cia2wiiu "My Game.cia" --layout gamepad
converter/target/release/cia2wiiu "My Game.cia" --format wup -o MyGame  # -> MyGame/{code,content,meta}
converter/target/release/cia2wiiu "My Game.cia" --format wup --pack --common-key wiiu_common_key.txt -o MyGame
                                                     # -> MyGame/{title.tmd,title.tik,title.cert,*.app,*.h3}
```

On Windows the program is `converter\target\release\cia2wiiu.exe`. It finds the kit in `kit/` next to or above itself, or in the current folder. `--kit` or `ZAKURO_WIIU_KIT` names another one.

Each conversion goes through these steps:

1. Decrypt the game, using your keys if it's encrypted, into a standalone `.cxi`.
2. Recompile its code to C and compile it for PowerPC. This is the slow step.
3. Link it with the kit into an RPX.
4. Pack it, with the game's own icon and name taken from its SMDH.

**`.wuhb`:** copy it to `sd:/wiiu/apps/` and it shows up on the Wii U menu under Aroma.

**`--format wup`:** gives you `code/` (the RPX, `app.xml` and `cos.xml`), `content/game.cxi` and `meta/` (`meta.xml` plus icon and splash TGAs). That is an unpacked title. Add `--pack` to get an installable WUP instead (see below). To pack a folder you converted earlier without converting again, run `cia2wiiu pack <folder> -o <wup folder> --common-key <file>` (it takes `--title-key` and `--cert` too, and packs in seconds). NUSPacker works on the folder as well.

**`--format wup --pack`:** packs the title into a WUP the way NUSPacker does: `title.tmd`, `title.tik`, `title.cert`, the encrypted contents `00000000.app` … `0000000X.app`, and an `.h3` hash table for each hashed content. Only the WUP goes in the output folder. The unpacked title goes in the work folder and is deleted afterwards, unless you pass `--keep`. Copy the folder to `sd:/install/` and install it with WUP Installer GX2. The console needs signature patches, which Aroma and Mocha provide, because the TMD and the ticket are not signed by Nintendo.

- **The Wii U common key.** Packing encrypts the title key with it, so you must supply it yourself. Pass `--common-key <file>` or set `WIIU_COMMON_KEY_FILE=<file>`. The file holds the 16-byte key, either as 16 raw bytes or as 32 hex digits (spaces and a newline are fine). It is part of your console's OTP, which dump tools such as dumpling save as `otp.bin`. Nothing here includes, downloads or prints the key.
- **The title key** that encrypts the contents is random for each pack. `--title-key <32 hex digits>` sets it. It is stored in the ticket, encrypted with the common key, and the converter never prints it.
- **`title.cert`.** Like NUSPacker, the converter writes a stand-in certificate chain with the right layout (CA00000003, signed by Root, then CP0000000b and XS0000000c, signed by Root-CA00000003) but no real keys or signatures. Signature-patched installs don't check it. If an installer rejects it anyway, copy `title.cert` from any title you dumped from your own console and pass it with `--cert <title.cert>`. The converter copies that file instead.

`--rom-on-sd` leaves the 3DS game out of the bundle. You then copy the `.cxi` it writes to `sd:/wiiu/zakuro/<title id>/game.cxi`.

Save data goes to `sd:/wiiu/zakuro/`.

## Layout of this repository

| Path | What it is |
|---|---|
| `zakuro/` | Zakuro, adapted to build for the Wii U target as well as the desktop |
| `3dsrecomp/` | 3dsrecomp, with big-endian-safe generated code, a `wiiu-code` mode and decryption |
| `runtime/` | the Rust runtime crate: the console on its own thread, frames, sound and input across the C boundary |
| `platform/` | the C layer: SDL2 video on TV and GamePad, layouts, input, sound, newlib shims, and the GX2 executor (`gx2.c`, `zw_gx2.h`) |
| `platform/shaders/` | the GX2 renderer's shaders; `raster.frag` is generated from Zakuro's by `tools/gx2_raster_frag.py` |
| `converter/` | `cia2wiiu`, the host tool |
| `target-spec/powerpc-wiiu-eabi.json` | the Rust target: Espresso (750), big-endian, hard float, newlib, and std's `horizon` path |
| `tools/build-kit.sh` | builds `kit/` (bash on Linux and macOS, devkitPro's MSYS2 on Windows) |
| `tools/build-shaders.sh` | compiles the GX2 shaders with CafeGLSL into the kit |
| `.github/workflows/build.yml` | CI: the host tools on Linux, Windows and macOS, and the kit in devkitPro's devkitPPC container |
| `testdata/hello-world/` | a libctru homebrew test ROM and its build script |

## Notes and limits

- **Graphics** are drawn on the Wii U's GPU through GX2: rasterization, texturing, lighting, the combiners and blending. Vertex shading stays on the CPU. Games fall back to Zakuro's software rasterizer when the kit was built without CafeGLSL, with `--renderer cpu`, or with `renderer=0` in the game's `wiiu.cfg`. Details and limits are in [docs/gx2.md](docs/gx2.md).
- **Code size decides the format.** Recompiled code is large: about 22 bytes of PowerPC per byte of ARM at `-O2`, and 23% less at `-Os`, which is the default (`--optimize speed` switches to `-O2`).
  - Aroma starts every `.wuhb` in place of Health & Safety, with a code area fixed at 64 MiB whatever the bundle says. About 56 MiB of that is usable for the program. The converter refuses to write a `.wuhb` past that limit; `--allow-large` writes it anyway.
  - An installed title (`--format wup`) sets its own `max_codesize` in `cos.xml`, up to the system's 224 MiB. The converter sizes it as the code + 10% + 8 MiB.
  - Big commercial games will usually need `--format wup`.
  - Cemu doesn't enforce the 64 MiB limit, so a `.wuhb` that is too big still runs there. On a console it fails with "Could not allocate uncompressed text".
- **Not supported on the Wii U:** local wireless and rooms, cameras, the microphone, texture packs and the 3D effect.
- **Distribution.** Converted games contain the game's code and data. They are for your own use only. Don't share them.
