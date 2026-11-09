#!/bin/sh
# builds libctru's hello-world example and wraps it as an unencrypted .cxi
# that 3dsrecomp reads: testdata/hello-world/hello.cxi. devkitPro's make
# rules break on paths with spaces, so it builds in a temporary directory.
set -e
here="$(cd "$(dirname "$0")" && pwd)"
export DEVKITPRO="${DEVKITPRO:-/opt/devkitpro}"
export DEVKITARM="${DEVKITARM:-$DEVKITPRO/devkitARM}"
export PATH="$DEVKITPRO/tools/bin:$DEVKITARM/bin:$PATH"
work="$(mktemp -d "${TMPDIR:-/tmp}/hello3ds.XXXXXX")"
trap 'rm -rf "$work"' EXIT
cp -R "$here/Makefile" "$here/source" "$here/hello.rsf" "$work/"
(cd "$work" && make TARGET=hello && makerom -f ncch -o hello.cxi -rsf hello.rsf -elf hello.elf -target t -ignoresign)
cp "$work/hello.elf" "$work/hello.cxi" "$here/"
ls -l "$here/hello.elf" "$here/hello.cxi"
