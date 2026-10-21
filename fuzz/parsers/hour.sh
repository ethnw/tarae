#!/bin/sh
# An hour of each fuzz target (README.md), on Linux, with the nightly in rust-toolchain.toml and cargo-fuzz installed.
# Run from this folder: ./hour.sh [seconds]   (3600 by default; JOBS targets at a time, one per core by default).
#
# Builds every target first, then runs them, each with its own corpus (corpus/<target>, grown and kept between runs)
# and the checked-in seeds (seeds/<target>, never written). A crash stops its target and leaves the input in
# artifacts/<target>/ (kept); a target's output is in logs/<target>.log. At the end: each target's crashes and
# libFuzzer's last coverage line. Exits 1 when any target found something.
set -eu
cd "$(dirname "$0")"
here=$(pwd)
root=$(cd ../.. && pwd)
secs=${1:-3600}
jobs=${JOBS:-$(nproc)}
channel=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
targets="sockaddr netlink msghdr head origin sni body dns_question dns_response"

[ "$(uname -s)" = Linux ] || { echo "hour.sh is written for Linux (README.md runs one target anywhere)" >&2; exit 2; }
[ -n "$channel" ] || { echo "no channel in rust-toolchain.toml" >&2; exit 2; }

# (from the repository's root, this folder named: cargo-fuzz's own layout is <project>/fuzz)
fuzz() { (cd "$root" && cargo "+$channel" fuzz "$@"); }

# Past each reader's limits where they're past libFuzzer's default (4096)
max_len() {
  case $1 in
    sockaddr) echo 256 ;;
    msghdr) echo 20000 ;;
    head | origin) echo 12288 ;;
    sni) echo 40000 ;;
    *) echo 8192 ;;
  esac
}

run_one() {
  t=$1
  mkdir -p "corpus/$t"
  echo "$(date +%T) $t: started"
  if fuzz run --fuzz-dir "$here" --debug-assertions "$t" "$here/corpus/$t" "$here/seeds/$t" -- \
    -max_total_time="$secs" -max_len="$(max_len "$t")" -timeout=10 -rss_limit_mb=2048 -print_final_stats=1 \
    >"logs/$t.log" 2>&1; then
    echo "$(date +%T) $t: done"
  else
    echo "$(date +%T) $t: stopped, exit $? (logs/$t.log)"
  fi
}

mkdir -p logs
echo "$(date +%T) building with $channel"
fuzz build --fuzz-dir "$here" --debug-assertions

n=0
for t in $targets; do
  run_one "$t" &
  n=$((n + 1))
  if [ "$n" -ge "$jobs" ]; then
    wait
    n=0
  fi
done
wait

echo
printf '%-14s %7s  %s\n' target found "last coverage"
found=0
for t in $targets; do
  c=$(find "artifacts/$t" -type f 2>/dev/null | wc -l)
  cov=$(grep -o 'cov: [0-9]* ft: [0-9]*' "logs/$t.log" | tail -n 1 || true)
  printf '%-14s %7s  %s\n' "$t" "$c" "${cov:--}"
  [ "$c" -eq 0 ] || found=1
done
exit "$found"
