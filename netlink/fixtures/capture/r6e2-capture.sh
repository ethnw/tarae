#!/bin/sh
# R6e2: nft's bytes for the session's own DNS server's rules (6e review): a question to port 53 a socket of the
# session's own (not transparent) is bound to is delivered to it — alone, and in 6e's whole batch: r6b-<vm>-6e2.txt
. "$(dirname "$0")/vm.sh"; { echo "ONLY=none"; cat "$HERE/r6b-inner.sh"; cat <<'EOF'
ONLY=; K='meta mark set meta mark or 0x1'; T4='tcp dport 53 tproxy ip to 192.0.2.1:15001 accept'; P='add rule inet tarae pre'
T6='tcp dport 53 tproxy ip6 to [2001:db8::1]:15001 accept'; O='dport 53 socket transparent 0 accept'
for p in udp tcp; do cap own-pre-dns-$p "$B" "$R pre '$p $O'"; done
sed -e "s/meta mark set 1\( return\)*;/$K\1;/" -e 's/ meta mark set 1 accept/ accept/' /tmp/split.nft > /tmp/s6.nft
head -3 /tmp/create.nft > /tmp/c6.nft; awk '/^  chain / {c=$2; next} /^  \}/ {c=""}
  c!="" && /^    / && !/^    type / {sub(/^ +/,""); sub(/;$/,""); print "add rule inet tarae " c " " $0}' /tmp/s6.nft >> /tmp/c6.nft
awk -v k="$K" -v t4="$T4" -v t6="$T6" -v o="$O" -v p="$P" '{print} / out udp dport 53 / {print "add rule inet tarae out tcp dport 53 " k " return"}
  / pre meta l4proto udp socket transparent 1 / {print p " udp " o; print p " tcp " o}
  / pre meta nfproto ipv6 udp dport 53 / {print p " meta nfproto ipv4 " t4; print p " meta nfproto ipv6 " t6}' /tmp/c6.nft > /tmp/c6e2.nft
echo; echo "--- /tmp/c6e2.nft"; cat /tmp/c6e2.nft; cap create-batch-6e2 '' "$D -f /tmp/c6e2.nft"
EOF
} | timeout 300 docker $RUN $IMAGE sh -s 2>&1 | tee "$LOGS/r6b-$VM-6e2.txt"
