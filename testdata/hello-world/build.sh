#!/bin/sh
# builds libctru's hello-world example and wraps it as an unencrypted .cxi
# that 3dsrecomp reads: testdata/hello-world/hello.cxi. devkitPro's make
# rules break on paths with spaces, so it builds in a temporary directory.
#
# needs devkitARM and libctru (dkp-pacman -S 3ds-dev, or pacman -S 3ds-dev in
# devkitPro's MSYS2 on Windows) and makerom from Project_CTR
# (https://github.com/3DSGuy/Project_CTR/releases) on the PATH or in
# $DEVKITPRO/tools/bin. runs under sh on Linux and macOS and in devkitPro's
# MSYS2 shell on Windows.
set -e
here="$(cd "$(dirname "$0")" && pwd)"

# the PATH takes MSYS2 paths on Windows, where DEVKITPRO may be C:/devkitPro
posix() { if command -v cygpath > /dev/null 2>&1; then cygpath -u "$1"; else printf '%s\n' "$1"; fi; }
DEVKITPRO="$(posix "${DEVKITPRO:-/opt/devkitpro}")"
DEVKITARM="$(posix "${DEVKITARM:-$DEVKITPRO/devkitARM}")"
export DEVKITPRO DEVKITARM
export PATH="$DEVKITPRO/tools/bin:$DEVKITARM/bin:$PATH"

for tool in make arm-none-eabi-gcc makerom; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "build.sh: no $tool; see the comment at the top of this script" >&2
    exit 1
  fi
done

# a temporary folder whose path has no spaces
tmp="${TMPDIR:-/tmp}"
case "$tmp" in *" "*) tmp=/tmp ;; esac
work="$(mktemp -d "${tmp%/}/hello3ds.XXXXXX")"
trap 'rm -rf "$work"' EXIT
cp -R "$here/Makefile" "$here/source" "$here/hello.rsf" "$work/"
(cd "$work" && make TARGET=hello && makerom -f ncch -o hello.cxi -rsf hello.rsf -elf hello.elf -target t -ignoresign)
cp "$work/hello.elf" "$work/hello.cxi" "$here/"
ls -l "$here/hello.elf" "$here/hello.cxi"
