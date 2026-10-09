#!/bin/sh
# ../netlink.txt from the capture logs (fixtures.awk): the cases fixtures.cases names — `sent` (the last send, what the
# bytes are held against) and `recv` (the answers the tests read). Without logs named, the committed one's: deb12's r6,
# r6b, r6b's `create` cases captured after (TAG=create), r6g's cgroup case, r6d's and r6e's. No VM needed.
#   sh fixtures.sh [log…]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd); LOGS=${LOGS:-$(cd "$HERE/../../.." && pwd)/target/sb/redirect}
[ $# -gt 0 ] || set -- "$LOGS/r6-deb12.txt" "$LOGS/r6b-deb12.txt" "$LOGS/r6b-deb12-create.txt" "$LOGS/r6b-deb12-cgroup.txt" \
  "$LOGS/r6b-deb12-6d.txt" "$LOGS/r6b-deb12-6d-rule.txt" "$LOGS/r6b-deb12-6e.txt" "$LOGS/r6b-deb12-6e2.txt"
list() { awk -v k="$1" '$1 == k { printf "%s%s", n++ ? "," : "", $2 }' "$HERE/fixtures.cases"; }
awk -v SENT="$(list sent)" -v RECV="$(list recv)" -f "$HERE/fixtures.awk" "$@" > "$HERE/../netlink.txt"
