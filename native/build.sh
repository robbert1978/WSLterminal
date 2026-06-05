#!/bin/sh
# Build the Linux-side PTY binaries *inside* WSL:
#   wslpty   - single-session helper (used by the console diagnostic modes)
#   wslptyd  - multiplexed PTY server (used by the GUI: one server, many PTYs)
# Output ELFs land in ../artifacts/ (on the Windows filesystem), from where the
# Windows host stages them into the distro at launch time. forkpty is in libutil.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
out="$here/../artifacts"
mkdir -p "$out"

CC="${CC:-cc}"
CFLAGS="${CFLAGS:-}"
echo "building with $CC ($($CC -dumpmachine))"

if [ -z "$CFLAGS" ]; then
    CFLAGS="-static -O3 -Wall -Wextra -std=c11"
else
    CFLAGS="${CFLAGS} -static -O3 -Wall -Wextra -std=c11"
fi

"$CC" $CFLAGS -o "$out/wslpty"  "$here/wslpty.c"  -lutil
"$CC" $CFLAGS -o "$out/wslptyd" "$here/wslptyd.c" -lutil

echo "built -> $out/wslpty , $out/wslptyd"
ls -l "$out/wslpty" "$out/wslptyd"
