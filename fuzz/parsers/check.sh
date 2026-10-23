#!/bin/sh
# Builds what the targets compile of tarae — src/lib.rs, the readers' modules under `--cfg fuzzing` — on the pinned
# stable, offline, without cargo-fuzz or libFuzzer: a copy of this folder with libfuzzer-sys and the [[bin]]s left out,
# in a temp folder (README.md "Checking without the nightly"). Run from anywhere: sh fuzz/parsers/check.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
d=$(mktemp -d "${TMPDIR:-/tmp}/fuzz-check.XXXX")
trap 'rm -rf "$d"' EXIT
mkdir -p "$d/fuzz/parsers"
cp -r "$here/src" "$d/fuzz/parsers/"
ln -s "$root/src" "$d/src"
# (the lib alone: no libFuzzer, no targets)
sed -e '/^libfuzzer-sys/d' -e '/^\[\[bin\]\]/,$d' "$here/Cargo.toml" >"$d/fuzz/parsers/Cargo.toml"
# (tarae's lock: the readers' crates at tarae's versions)
cp "$root/Cargo.lock" "$d/fuzz/parsers/Cargo.lock"
cd "$d/fuzz/parsers"
RUSTFLAGS="--cfg fuzzing -D warnings" CARGO_TARGET_DIR="$d/t" CARGO_BUILD_BUILD_DIR="$d/b" \
  cargo +"$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$root/rust-toolchain.toml")" build --offline --lib
echo "fuzz/parsers: the targets' library builds under --cfg fuzzing"
