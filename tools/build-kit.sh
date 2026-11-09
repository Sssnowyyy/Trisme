#!/usr/bin/env bash
# builds the Zakuro Wii U kit: the Rust runtime as a static library for the
# Wii U, the C platform layer's objects, and the headers and template the
# converter uses for each game. output: kit/ at the repository root.
#
# needs: rustup with a nightly toolchain and rust-src
#        (rustup toolchain install nightly --profile minimal -c rust-src),
#        devkitPro with devkitPPC, wut and wiiu-sdl2.
#
# runs under bash on Linux and macOS, and on Windows in the MSYS2 shell that
# comes with devkitPro (or any MSYS2/Git Bash with cygpath). there cargo,
# rustc and powerpc-eabi-gcc are native Windows programs, so the paths they
# are given are converted to Windows ones (C:/...) with cygpath.
set -euo pipefail

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) windows=1 ;;
  *) windows=0 ;;
esac

# a path as native programs take it (C:/x on Windows), and as the shell's
# PATH takes it (/c/x there)
native() { if [ "$windows" = 1 ]; then cygpath -m "$1"; else printf '%s\n' "$1"; fi; }
posix() { if [ "$windows" = 1 ]; then cygpath -u "$1"; else printf '%s\n' "$1"; fi; }

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# devkitPro's own shells set DEVKITPRO=/opt/devkitpro (an MSYS2 path on
# Windows); a Windows-style C:/devkitPro or C:\devkitPro works too
DEVKITPRO="$(posix "${DEVKITPRO:-/opt/devkitpro}")"
DEVKITPPC="$(posix "${DEVKITPPC:-$DEVKITPRO/devkitPPC}")"
export DEVKITPRO DEVKITPPC
export PATH="$DEVKITPPC/bin:$DEVKITPRO/tools/bin:$PATH"

# cargo and rustup: rustup puts them in ~/.cargo/bin (%USERPROFILE%\.cargo\bin
# on Windows, which devkitPro's MSYS2 leaves out of its PATH); Homebrew's
# rustup is keg-only on macOS
for dir in "${CARGO_HOME:+$(posix "$CARGO_HOME")/bin}" "$HOME/.cargo/bin" \
           "${USERPROFILE:+$(posix "$USERPROFILE")/.cargo/bin}"; do
  if [ -n "$dir" ] && [ -d "$dir" ]; then export PATH="$PATH:$dir"; fi
done
if [ -d /opt/homebrew/opt/rustup/bin ]; then export PATH="/opt/homebrew/opt/rustup/bin:$PATH"; fi

kit="$(posix "${KIT_DIR:-$root/kit}")"
target_dir="$(posix "${CARGO_TARGET_DIR:-$root/target/wiiu}")"
spec="$root/target-spec/powerpc-wiiu-eabi.json"
toolchain="${ZAKURO_WIIU_RUST:-nightly}"

missing() { echo "build-kit.sh: $*" >&2; exit 1; }
command -v powerpc-eabi-gcc > /dev/null ||
  missing "no powerpc-eabi-gcc in $DEVKITPPC/bin; install devkitPPC (dkp-pacman -S wiiu-dev, or pacman -S wiiu-dev in devkitPro's MSYS2) or set DEVKITPRO"
[ -f "$DEVKITPRO/wut/include/wut.h" ] || missing "no wut in $DEVKITPRO/wut; install it with wiiu-dev"
[ -f "$DEVKITPRO/portlibs/wiiu/include/SDL2/SDL.h" ] || missing "no SDL2 for the Wii U in $DEVKITPRO/portlibs/wiiu; install wiiu-sdl2"
command -v cargo > /dev/null || missing "no cargo; install Rust with rustup (https://rustup.rs)"
command -v rustup > /dev/null ||
  missing "no rustup; cargo +$toolchain needs rustup's cargo, not a distribution's (rustup toolchain install nightly --profile minimal -c rust-src)"

echo "== runtime (Rust, $toolchain) for the Wii U"
(
  cd "$root/runtime"
  # std's build script marks unknown targets' std as unstable unless told
  # this is a synthetic target; the spec is std's newlib path on PowerPC.
  # cc-rs finds the cross compiler and ar on the PATH by these names, with
  # .exe on Windows
  RUSTC_BOOTSTRAP_SYNTHETIC_TARGET=1 \
  CARGO_TARGET_DIR="$(native "$target_dir")" \
  CC_powerpc_wiiu_eabi=powerpc-eabi-gcc AR_powerpc_wiiu_eabi=powerpc-eabi-ar \
  CFLAGS_powerpc_wiiu_eabi="-mcpu=750 -meabi -mhard-float -O2" \
  cargo "+$toolchain" build --release \
    -Z json-target-spec \
    -Z build-std=std,panic_abort \
    -Z build-std-features=compiler-builtins-mem \
    --target "$(native "$spec")"
)

echo "== platform layer (C)"
mkdir -p "$kit/lib" "$kit/obj" "$kit/include" "$kit/share"
dkp="$(native "$DEVKITPRO")"
cflags=(-O2 -Wall -Wextra -Wno-unused-parameter -DESPRESSO -mcpu=750 -meabi -mhard-float -D__WIIU__ -D__WUT__
        -I"$dkp/wut/include" -I"$dkp/portlibs/wiiu/include" -I"$dkp/portlibs/wiiu/include/SDL2"
        -I"$(native "$root/platform")")
for source in main layout newlib_shims gx2; do
  powerpc-eabi-gcc "${cflags[@]}" -c "$(native "$root/platform/$source.c")" -o "$(native "$kit/obj/$source.o")"
done

echo "== GX2 shaders"
# compiled ahead of time with CafeGLSL; without it the kit still builds and
# games draw on the CPU
"$root/tools/build-shaders.sh" "$kit/obj/gx2_shaders.o"

cp "$target_dir/powerpc-wiiu-eabi/release/libzakuro_wiiu.a" "$kit/lib/"
cp "$root/platform/zakuro_wiiu.h" "$root/platform/game_info.h" "$root/platform/layout.h" "$root/platform/zw_gx2.h" "$kit/include/"
cp "$root/platform/game_info_template.c" "$kit/share/"
git -C "$root" rev-parse --short HEAD > "$kit/VERSION" 2>/dev/null || echo unknown > "$kit/VERSION"
echo "kit ready in $kit ($(du -sh "$kit" | cut -f1))"
