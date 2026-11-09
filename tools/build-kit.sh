#!/usr/bin/env bash
# builds the Zakuro Wii U kit: the Rust runtime as a static library for the
# Wii U, the C platform layer's objects, and the headers and template the
# converter uses for each game. output: kit/ at the repository root.
#
# needs: rustup with a nightly toolchain and rust-src
#        (rustup toolchain install nightly --profile minimal -c rust-src),
#        devkitPro with devkitPPC, wut and wiiu-sdl2.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export DEVKITPRO="${DEVKITPRO:-/opt/devkitpro}"
export DEVKITPPC="${DEVKITPPC:-$DEVKITPRO/devkitPPC}"
export PATH="$DEVKITPPC/bin:$DEVKITPRO/tools/bin:$PATH"
if [ -d /opt/homebrew/opt/rustup/bin ]; then export PATH="/opt/homebrew/opt/rustup/bin:$PATH"; fi

kit="${KIT_DIR:-$root/kit}"
target_dir="${CARGO_TARGET_DIR:-$root/target/wiiu}"
spec="$root/target-spec/powerpc-wiiu-eabi.json"
toolchain="${ZAKURO_WIIU_RUST:-nightly}"

echo "== runtime (Rust, $toolchain) for the Wii U"
(
  cd "$root/runtime"
  # std's build script marks unknown targets' std as unstable unless told
  # this is a synthetic target; the spec is std's newlib path on PowerPC
  RUSTC_BOOTSTRAP_SYNTHETIC_TARGET=1 \
  CARGO_TARGET_DIR="$target_dir" \
  CC_powerpc_wiiu_eabi=powerpc-eabi-gcc AR_powerpc_wiiu_eabi=powerpc-eabi-ar \
  CFLAGS_powerpc_wiiu_eabi="-mcpu=750 -meabi -mhard-float -O2" \
  cargo "+$toolchain" build --release \
    -Z json-target-spec \
    -Z build-std=std,panic_abort \
    -Z build-std-features=compiler-builtins-mem \
    --target "$spec"
)

echo "== platform layer (C)"
mkdir -p "$kit/lib" "$kit/obj" "$kit/include" "$kit/share"
cflags=(-O2 -Wall -Wextra -Wno-unused-parameter -DESPRESSO -mcpu=750 -meabi -mhard-float -D__WIIU__ -D__WUT__
        -I"$DEVKITPRO/wut/include" -I"$DEVKITPRO/portlibs/wiiu/include" -I"$DEVKITPRO/portlibs/wiiu/include/SDL2"
        -I"$root/platform")
for source in main layout newlib_shims; do
  powerpc-eabi-gcc "${cflags[@]}" -c "$root/platform/$source.c" -o "$kit/obj/$source.o"
done

cp "$target_dir/powerpc-wiiu-eabi/release/libzakuro_wiiu.a" "$kit/lib/"
cp "$root/platform/zakuro_wiiu.h" "$root/platform/game_info.h" "$root/platform/layout.h" "$kit/include/"
cp "$root/platform/game_info_template.c" "$kit/share/"
git -C "$root" rev-parse --short HEAD > "$kit/VERSION" 2>/dev/null || echo unknown > "$kit/VERSION"
echo "kit ready in $kit ($(du -sh "$kit" | cut -f1))"
