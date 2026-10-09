#!/bin/sh
# R6d: nft's bytes for 6d's rules (marks kept, bit 0 added; none set in prerouting), alone and as one batch: r6b-<vm>-6d.txt
. "$(dirname "$0")/vm.sh"; { echo "ONLY=none"; cat "$HERE/r6b-inner.sh"; cat <<'EOF'
ONLY=; K='meta mark set meta mark or 0x1'; P='meta nfproto ipv'; T='tproxy ip to 192.0.2.1'; T6='tproxy ip6 to [2001:db8::1]'
for p in tcp udp; do cap keep-out-$p "$B" "$R out 'meta l4proto $p $K'"
  cap keep-pre-socket-$p "$B" "$R pre 'meta l4proto $p socket transparent 1 accept'"; done
cap keep-out-dns "$B" "$R out 'udp dport 53 $K return'"
for v in 4 6; do a=$T; [ $v = 6 ] && a=$T6
  cap keep-pre-dns$v "$B" "$R pre '$P$v udp dport 53 $a:15053 accept'"; cap keep-pre-tcp$v "$B" "$R pre '$P$v meta l4proto tcp $a:15001 accept'"
  cap keep-pre-udp$v "$B" "$R pre '$P$v meta l4proto udp $a:15053 accept'"; done
sed -e "s/meta mark set 1\( return\)*;/$K\1;/" -e 's/ meta mark set 1 accept/ accept/' /tmp/split.nft > /tmp/s6.nft; head -3 /tmp/create.nft > /tmp/c6.nft
awk '/^  chain / {c=$2; next} /^  \}/ {c=""}
  c!="" && /^    / && !/^    type / {sub(/^ +/,""); sub(/;$/,""); print "add rule inet tarae " c " " $0}' /tmp/s6.nft >> /tmp/c6.nft
echo; echo "--- /tmp/c6.nft"; cat /tmp/c6.nft; cap create-batch-6d '' "$D -f /tmp/c6.nft"
EOF
} | timeout 300 docker $RUN $IMAGE sh -s 2>&1 | tee "$LOGS/r6b-$VM-6d.txt"
