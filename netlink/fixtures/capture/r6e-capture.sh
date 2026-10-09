#!/bin/sh
# R6e: nft's bytes for 6e's TCP-53 rules (TCP to any port 53, the loopback resolvers too, kept for the session: marked
# in output, to the planted TCP socket in prerouting), alone and in 6e's whole batch: r6b-<vm>-6e.txt
. "$(dirname "$0")/vm.sh"; { echo "ONLY=none"; cat "$HERE/r6b-inner.sh"; cat <<'EOF'
ONLY=; K='meta mark set meta mark or 0x1'; T4='tcp dport 53 tproxy ip to 192.0.2.1:15001 accept'
T6='tcp dport 53 tproxy ip6 to [2001:db8::1]:15001 accept'; cap keep-out-dns-tcp "$B" "$R out 'tcp dport 53 $K return'"
cap keep-pre-dns-tcp4 "$B" "$R pre 'meta nfproto ipv4 $T4'"; cap keep-pre-dns-tcp6 "$B" "$R pre 'meta nfproto ipv6 $T6'"
sed -e "s/meta mark set 1\( return\)*;/$K\1;/" -e 's/ meta mark set 1 accept/ accept/' /tmp/split.nft > /tmp/s6.nft
head -3 /tmp/create.nft > /tmp/c6.nft; awk '/^  chain / {c=$2; next} /^  \}/ {c=""}
  c!="" && /^    / && !/^    type / {sub(/^ +/,""); sub(/;$/,""); print "add rule inet tarae " c " " $0}' /tmp/s6.nft >> /tmp/c6.nft
awk -v k="$K" -v t4="$T4" -v t6="$T6" '{print} / out udp dport 53 / {print "add rule inet tarae out tcp dport 53 " k " return"}
  / pre meta nfproto ipv6 udp dport 53 / {print "add rule inet tarae pre meta nfproto ipv4 " t4
  print "add rule inet tarae pre meta nfproto ipv6 " t6}' /tmp/c6.nft > /tmp/c6e.nft
echo; echo "--- /tmp/c6e.nft"; cat /tmp/c6e.nft; cap create-batch-6e '' "$D -f /tmp/c6e.nft"
EOF
} | timeout 300 docker $RUN $IMAGE sh -s 2>&1 | tee "$LOGS/r6b-$VM-6e.txt"
