# Wii U

`wiiu-code` writes a title's code as C, the same C that `build` and `port`
write, and compiles it with devkitPPC's `powerpc-eabi-gcc` into a static
library that a Wii U program links in:

```
3dsrecomp wiiu-code <rom> <dir> [--overrides <file or dir>]
```

It writes:

- `<dir>/code/*.c`, `recomp.h`, `functions.h`, and the overrides, the same
  files `port` writes;
- `<dir>/code/*.o` and `<dir>/code/librecomp.a`, the objects for the Wii U;
- `<dir>/code/info.json`, which holds the title's name, its title id (the
  program id), the `recomp_abi` and `recomp_generation` values, the entry
  and module counts, the symbols and their C types, and the flags used.

It writes no Cargo project. The program that links the archive is built
separately.

From Rust, `recomp3ds::wiiu::write_wiiu_code(rom, dir, overrides)` does the
same thing and returns a `CodeInfo`. `write_wiiu_code_with` also takes a
mod's `Mods` and an event callback, the way `build::build` does.

## Toolchain

The compiler is the first of these that runs:

1. `WIIU_CC`
2. `$DEVKITPPC/bin/powerpc-eabi-gcc`
3. `$DEVKITPRO/devkitPPC/bin/powerpc-eabi-gcc`
4. `/opt/devkitpro/devkitPPC/bin/powerpc-eabi-gcc`
5. `powerpc-eabi-gcc` on the `PATH`

`ar` and `nm` are taken from the same directory as the compiler, or else
`powerpc-eabi-ar` and `powerpc-eabi-nm` from the `PATH`. Files are compiled
in parallel, the same way the host build does it. They are then archived
with `ar rcs`, 256 objects per call. Finally `nm` checks that the archive
defines the symbols below.

## Flags

```
-O2 -ffp-contract=off -fno-math-errno -w
-mcpu=750 -meabi -mhard-float -mlongcall
-ffunction-sections -fdata-sections -fno-strict-aliasing -fvisibility=hidden
```

and, because the compiler is gcc,
`--param ggc-min-expand=20 --param ggc-min-heapsize=65536`.

- **`-ffp-contract=off`** matters even more here than on the host. GCC fuses
  multiplies and adds into `fmadd` on PowerPC by default, and a fused result
  rounds differently from what the interpreter computes.
- **`-mlongcall`** turns every call into `mtctr`/`bctrl`. A big game produces
  tens of MiB of code, and a PowerPC `bl` only reaches ±32 MiB. The objects
  contain no `R_PPC_REL24` relocations.
- **No `-fPIC` and no `-shared`.** An RPX is linked statically.

The program linking the archive needs `-lm`. `sqrt` is a libm call on the
750, which has no `fsqrt`. It also needs libgcc, which gcc links by default,
for the 64-bit divisions.

## Symbols

The program passes these to `zakuro::Linked::new(...).with_origins(...)`,
the same way the `main.rs` that `port` generates does:

| symbol | C type |
| --- | --- |
| `recomp_abi` | `const uint32_t` |
| `recomp_generation` | `const uint32_t` |
| `recomp_entry_count` | `const uint32_t` |
| `recomp_entries` | `const Entry[]` (`recomp_entry_count` of them) |
| `recomp_module_count` | `const uint32_t` |
| `recomp_modules` | `const Module[]` (`recomp_module_count` of them) |
| `recomp_origins` | `const Origins[]` (the executable's, then one per module) |
| `recomp_natives` | `uint8_t`, only present when the title has natives (see overrides.md) |

All of them are defined in `entries.o`, except `recomp_natives`, which is
defined in `natives.o`. `recomp_stale` and `recomp_starts` also show up in
`nm`. They have hidden visibility and are only for the generated code.

## Byte order

The Wii U is big-endian. The 3DS, and so guest memory, is little-endian.
`recomp.h` byte-swaps every halfword and word that the code moves between a
page and a host integer: `mem_read16`/`32`, `mem_write16`/`32`, and the
`load32`/`store32` that `ldm`, `stm`, `vldm` and `vstm` use. GCC compiles
these to `lhbrx`, `lwbrx`, `sthbrx` and `stwbrx`, one instruction each. A
64-bit access (`ldrd`/`strd`, `ldrexd`/`strexd`, or a double in
`vldr`/`vstr`) is two word accesses, with the low word at the lower address.
VFP registers are 32-bit words. A double is put together from two of them
with shifts, so its layout in host memory never matters. On a little-endian
host the swaps compile to nothing, and the objects are byte-for-byte what
they were before.

The callbacks a host provides (`read16`, `write32`, ...) take and return
host integers, so they need no swapping. The host is responsible for
building those integers from the page bytes in little-endian order.

The structs in `recomp.h` have the same layout under `powerpc-eabi-gcc
-meabi` as the `#[repr(C)]` mirrors in `abi/src/lib.rs` on the Wii U target:
4-byte pointers, and `uint64_t` aligned to 8 (`Piece` is 16 bytes, with its
hash at 8). The unit test `wiiu::tests::the_header_swaps_guest_memory_on_the_wii_u`
checks the sizes and the byte-reversed loads and stores, when devkitPPC is
installed.

## Testing without a game

`testdata/hello-world/build.sh` builds libctru's hello-world example with
devkitARM. It then uses `makerom` and a minimal RSF to wrap it as an
unencrypted `hello.cxi`, which `wiiu-code` and `build` both accept. This
only shows that the code compiles. Nothing here runs it.
