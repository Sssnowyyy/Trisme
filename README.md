# Zakuro for Wii U

This turns a 3DS game you own (`.cia`, `.3ds` or `.cxi`) into a Wii U title. The output is either a `.wuhb` you start from the Wii U menu under Aroma, or an unpacked title folder that NUSPacker turns into an installable WUP.

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

- devkitPro with `devkitPPC`, `wut`, `wut-tools` and `wiiu-sdl2`, plus `devkitARM`/`libctru` if you want to build the homebrew test ROM.
- Rust, through rustup, with a nightly toolchain and `rust-src`. The Wii U target builds `std` with `-Z build-std`:
  ```
  rustup toolchain install nightly --profile minimal -c rust-src
  ```
- Your own console's keys, if the dump is encrypted, which retail `.cia` dumps (`*.legit.cia`) are. Put a Citra/Azahar-format `aes_keys.txt` next to the game or in Azahar's `sysdata` folder, or pass `--keys`. You can dump the keys from your own 3DS with GodMode9. Nothing here includes keys. Seed-encrypted titles also need `seeddb.bin`.

## Building

```
tools/build-kit.sh                     # the runtime and platform layer for the Wii U -> kit/
cd converter && cargo build --release  # the converter, for your computer
```

## Converting a game

```
converter/target/release/cia2wiiu "My Game.cia"                        # -> My_Game.wuhb
converter/target/release/cia2wiiu "My Game.cia" --layout gamepad
converter/target/release/cia2wiiu "My Game.cia" --format wup -o MyGame  # -> MyGame/{code,content,meta}
```

Each conversion goes through these steps:

1. Decrypt the game, using your keys if it's encrypted, into a standalone `.cxi`.
2. Recompile its code to C and compile it for PowerPC. This is the slow step.
3. Link it with the kit into an RPX.
4. Pack it, with the game's own icon and name taken from its SMDH.

**`.wuhb`:** copy it to `sd:/wiiu/apps/` and it shows up on the Wii U menu under Aroma.

**`--format wup`:** gives you `code/` (the RPX, `app.xml` and `cos.xml`), `content/game.cxi` and `meta/` (`meta.xml` plus icon and splash TGAs). Pack that with NUSPacker and install the result with WUP Installer GX2 or a similar tool. Packing needs the Wii U common key, which you supply yourself.

`--rom-on-sd` leaves the 3DS game out of the bundle. You then copy the `.cxi` it writes to `sd:/wiiu/zakuro/<title id>/game.cxi`.

Save data goes to `sd:/wiiu/zakuro/`.

## Layout of this repository

| Path | What it is |
|---|---|
| `zakuro/` | Zakuro, adapted to build for the Wii U target as well as the desktop |
| `3dsrecomp/` | 3dsrecomp, with big-endian-safe generated code, a `wiiu-code` mode and decryption |
| `runtime/` | the Rust runtime crate: the console on its own thread, frames, sound and input across the C boundary |
| `platform/` | the C layer: SDL2 video on TV and GamePad, layouts, input, sound, newlib shims |
| `converter/` | `cia2wiiu`, the host tool |
| `target-spec/powerpc-wiiu-eabi.json` | the Rust target: Espresso (750), big-endian, hard float, newlib, and std's `horizon` path |
| `tools/build-kit.sh` | builds `kit/` |
| `testdata/hello-world/` | a libctru homebrew test ROM and its build script |

## Notes and limits

- **Graphics** are drawn by Zakuro's software rasterizer on the CPU. The Vulkan renderer isn't available on the Wii U, and a GX2 renderer would be the next big step for speed.
- **Code size decides the format.** Recompiled code is large: about 22 bytes of PowerPC per byte of ARM at `-O2`, and 23% less at `-Os`, which is the default (`--optimize speed` switches to `-O2`).
  - Aroma starts every `.wuhb` in place of Health & Safety, with a code area fixed at 64 MiB whatever the bundle says. About 56 MiB of that is usable for the program. The converter refuses to write a `.wuhb` past that limit; `--allow-large` writes it anyway.
  - An installed title (`--format wup`) sets its own `max_codesize` in `cos.xml`, up to the system's 224 MiB. The converter sizes it as the code + 10% + 8 MiB.
  - Big commercial games will usually need `--format wup`.
  - Cemu doesn't enforce the 64 MiB limit, so a `.wuhb` that is too big still runs there. On a console it fails with "Could not allocate uncompressed text".
- **Not supported on the Wii U:** local wireless and rooms, cameras, the microphone, texture packs and the 3D effect.
- **Distribution.** Converted games contain the game's code and data. They are for your own use only. Don't share them.
