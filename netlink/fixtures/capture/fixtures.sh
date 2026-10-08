#!/bin/sh
# ../netlink.txt from the capture logs (fixtures.awk): the cases whose last send the crate's bytes are held against
# (SENT), and those whose answers the tests read or seed the parser loop with (RECV). Without logs named, the committed
# one's: deb12's r6, r6b, r6b's `create` cases captured after (TAG=create) and r6g's cgroup case. No VM needed.
#   sh fixtures.sh [log…]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd); LOGS=${LOGS:-$(cd "$HERE/../../.." && pwd)/target/sb/redirect}
[ $# -gt 0 ] || set -- "$LOGS/r6-deb12.txt" "$LOGS/r6b-deb12.txt" "$LOGS/r6b-deb12-create.txt" "$LOGS/r6b-deb12-cgroup.txt"
SENT=create-table,create-table-owner,create-chain-route,create-chain-prerouting,create-batch,rule-out-dns,rule-pre-dns4
SENT=$SENT,rule-pre-dns6,rule-pre-tcp4,rule-pre-tcp6,rule-pre-udp4,rule-pre-udp6,split-out-lo4,split-out-session4
SENT=$SENT,split-out-lo6,split-out-session6,split-out-tcp,split-out-udp,split-out-dns,split-pre-socket-tcp
SENT=$SENT,split-pre-socket-udp,split-pre-lo4,split-pre-session4,split-pre-lo6,split-pre-session6,pre-drop
SENT=$SENT,pre-socket-plain,route-default4,route-default6,route-local4,route-local6,rule-fwmark4,rule-fwmark6,rule-fwmark4-mask,split-out-cgroup
RECV=err-exists,err-eperm,err-route-exists,err-exists-again,err-no-chain,err-route-prefix,dump-rules,dump-routes4
RECV=$RECV,dump-routes6,dump-rules4,dump-rules6,route-default4,rule-fwmark4,err-chain-exists
awk -v SENT="$SENT" -v RECV="$RECV" -f "$HERE/fixtures.awk" "$@" > "$HERE/../netlink.txt"
