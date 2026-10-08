# R6, inside the container (uid 1000, no capabilities): the netlink messages `nft` and `ip` send for the redirect's
# rules and routes — TPROXY's (the preferred path) first, DNAT's as the fallback — and the kernel's answers, as
# fixtures for tarae-netlink's byte-exact goldens and fuzz seeds. Each case in a fresh `unshare -Urn` (a user + net
# namespace of its own, root-mapped there): the session's setup and what the case needs before it, untraced, then its
# one command under strace — every netlink message sent and received, whole (strace's hex dump) — with nft's own
# decoding beside it (--debug=netlink). Only namespaces this script makes are touched; nothing leaves them.
echo "kernel $(uname -r) · $(nft --version) · $(ip -V) · $(strace -V | head -1)"
cat > /tmp/setup.sh <<'EOF'
ip link set lo up; ip addr add 192.0.2.1/32 dev lo; ip -6 addr add 2001:db8::1/128 dev lo nodad
EOF
cat > /tmp/base.nft <<'EOF'
add table inet rd
add chain inet rd out { type route hook output priority -150; policy accept; }
add chain inet rd pre { type filter hook prerouting priority -150; policy accept; }
add chain inet rd nat { type nat hook output priority -100; policy accept; }
EOF
# t1's tproxy-socket ruleset, whole: the one batch tarae would send
cat > /tmp/tproxy.nft <<'EOF'
table inet rd {
  chain out {
    type route hook output priority -150; policy accept;
    udp dport 53 meta mark set 1 return;
    ip daddr { 127.0.0.0/8, 192.0.2.1 } return;
    ip6 daddr { ::1, 2001:db8::1 } return;
    meta l4proto { tcp, udp } meta mark set 1;
  }
  chain pre {
    type filter hook prerouting priority -150; policy accept;
    meta l4proto { tcp, udp } socket transparent 1 meta mark set 1 accept;
    meta nfproto ipv4 udp dport 53 tproxy ip to 192.0.2.1:15053 meta mark set 1 accept;
    meta nfproto ipv6 udp dport 53 tproxy ip6 to [2001:db8::1]:15053 meta mark set 1 accept;
    ip daddr { 127.0.0.0/8, 192.0.2.1 } return;
    ip6 daddr { ::1, 2001:db8::1 } return;
    meta nfproto ipv4 meta l4proto tcp tproxy ip to 192.0.2.1:15001 meta mark set 1 accept;
    meta nfproto ipv6 meta l4proto tcp tproxy ip6 to [2001:db8::1]:15001 meta mark set 1 accept;
    meta nfproto ipv4 meta l4proto udp tproxy ip to 192.0.2.1:15053 meta mark set 1 accept;
    meta nfproto ipv6 meta l4proto udp tproxy ip6 to [2001:db8::1]:15053 meta mark set 1 accept;
  }
}
EOF
# cap NAME BEFORE COMMAND: in a fresh namespace, the setup and BEFORE untraced, then COMMAND traced
cap() {
  printf '%s\n' "$2" > /tmp/before.sh
  echo; echo "=== $1"; echo "--- before: ${2:-(the setup only)}"; echo "--- traced: $3"
  unshare -Urn sh -c '
    { sh -e /tmp/setup.sh && sh -e /tmp/before.sh; } >/dev/null 2>&1 || echo "(the setup or what comes before failed)"
    strace -f -qq -e trace=sendto,sendmsg,recvfrom,recvmsg -e write=all -e read=all -o /tmp/st.txt sh -c "$1" 2>&1
    echo "--- exit $?"; echo "--- strace"; cat /tmp/st.txt' _ "$3"
}
B='nft -f /tmp/base.nft'
T='nft add table inet rd'
D='nft --debug=netlink'

# nf_tables — TPROXY: the table (and one owned by its socket: NFT_TABLE_F_OWNER, gone when nft exits), its chains
cap table '' "$D add table inet rd"
cap table-owner '' "$D add table inet rd '{ flags owner; }'"
cap chain-route "$T" "$D add chain inet rd out '{ type route hook output priority -150; policy accept; }'"
cap chain-prerouting "$T" "$D add chain inet rd pre '{ type filter hook prerouting priority -150; policy accept; }'"
# … each rule alone
cap rule-out-dns "$B" "$D add rule inet rd out 'udp dport 53 meta mark set 1 return'"
cap rule-out-session4 "$B" "$D add rule inet rd out 'ip daddr { 127.0.0.0/8, 192.0.2.1 } return'"
cap rule-out-session6 "$B" "$D add rule inet rd out 'ip6 daddr { ::1, 2001:db8::1 } return'"
cap rule-out-mark "$B" "$D add rule inet rd out 'meta l4proto { tcp, udp } meta mark set 1'"
cap rule-pre-socket "$B" "$D add rule inet rd pre 'meta l4proto { tcp, udp } socket transparent 1 meta mark set 1 accept'"
cap rule-pre-dns4 "$B" "$D add rule inet rd pre 'meta nfproto ipv4 udp dport 53 tproxy ip to 192.0.2.1:15053 meta mark set 1 accept'"
cap rule-pre-dns6 "$B" "$D add rule inet rd pre 'meta nfproto ipv6 udp dport 53 tproxy ip6 to [2001:db8::1]:15053 meta mark set 1 accept'"
cap rule-pre-tcp4 "$B" "$D add rule inet rd pre 'meta nfproto ipv4 meta l4proto tcp tproxy ip to 192.0.2.1:15001 meta mark set 1 accept'"
cap rule-pre-tcp6 "$B" "$D add rule inet rd pre 'meta nfproto ipv6 meta l4proto tcp tproxy ip6 to [2001:db8::1]:15001 meta mark set 1 accept'"
cap rule-pre-udp4 "$B" "$D add rule inet rd pre 'meta nfproto ipv4 meta l4proto udp tproxy ip to 192.0.2.1:15053 meta mark set 1 accept'"
cap rule-pre-udp6 "$B" "$D add rule inet rd pre 'meta nfproto ipv6 meta l4proto udp tproxy ip6 to [2001:db8::1]:15053 meta mark set 1 accept'"
# … the whole ruleset as one batch, and a whole replacement's delete
cap tproxy-batch '' "$D -f /tmp/tproxy.nft"
cap delete-table "$B" "$D delete table inet rd"
# nf_tables — DNAT, the fallback
cap rule-nat-dnat4 "$B" "$D add rule inet rd nat 'meta nfproto ipv4 meta l4proto tcp dnat ip to 192.0.2.1:15001'"
cap rule-nat-dnat6 "$B" "$D add rule inet rd nat 'meta nfproto ipv6 meta l4proto tcp dnat ip6 to [2001:db8::1]:15001'"
# nf_tables — the kernel's errors (fuzz seeds: NLMSG_ERROR, with an extended ack where the kernel gives one)
cap err-exists "$B" 'nft create table inet rd'
cap err-eperm "$B" 'unshare -Ur nft add rule inet rd pre accept'

# rtnetlink — the routes via lo, TPROXY's fwmark rule (with a mask too: FRA_FWMASK) and its local route
cap route-default4 '' 'ip route add default dev lo'
cap route-default6 '' 'ip -6 route add default dev lo'
cap rule-fwmark4 '' 'ip rule add fwmark 1 lookup 100'
cap rule-fwmark4-mask '' 'ip rule add fwmark 1/0xff lookup 100'
cap rule-fwmark6 '' 'ip -6 rule add fwmark 1 lookup 100'
cap route-local4 '' 'ip route add local default dev lo table 100'
cap route-local6 '' 'ip -6 route add local default dev lo table 100'
cap err-route-exists 'ip route add default dev lo' 'ip route add default dev lo'
