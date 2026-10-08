# R6b, inside the container (uid 1000, no capabilities): r6's capture again for what tarae-netlink builds that r6
# didn't capture — the session's ruleset written without sets (one rule per element, a prefix as its own rule), the
# final drop, `socket transparent 0`, the error replies the kernel gives with an extended ack (an attribute's offset, a
# text), and the dumps tarae reads back (rules, routes, policy rules). Each case in a fresh `unshare -Urn` (a user + net
# namespace of its own, root-mapped there), as r6: the setup and what the case needs before it untraced, then its one
# command under strace — every netlink message sent and received, whole, and the socket options set (setsockopt:
# NETLINK_EXT_ACK and the rest). Only namespaces this script makes are touched; nothing leaves them.
echo "kernel $(uname -r) · $(nft --version) · $(ip -V) · $(strace -V | head -1)"
cat > /tmp/setup.sh <<'EOF'
ip link set lo up; ip addr add 192.0.2.1/32 dev lo; ip -6 addr add 2001:db8::1/128 dev lo nodad
EOF
# the table and chains tarae makes (named as tarae names them), without the owner flag so `before` can hold them
cat > /tmp/base.nft <<'EOF'
add table inet tarae
add chain inet tarae out { type route hook output priority -150; policy accept; }
add chain inet tarae pre { type filter hook prerouting priority -150; policy accept; }
EOF
# the session's whole ruleset as tarae will send it — t1's, its sets split into one rule per element, the final drop
cat > /tmp/split.nft <<'EOF'
table inet tarae {
  flags owner;
  chain out {
    type route hook output priority -150; policy accept;
    udp dport 53 meta mark set 1 return;
    ip daddr 127.0.0.0/8 return;
    ip daddr 192.0.2.1 return;
    ip6 daddr ::1 return;
    ip6 daddr 2001:db8::1 return;
    meta l4proto tcp meta mark set 1;
    meta l4proto udp meta mark set 1;
  }
  chain pre {
    type filter hook prerouting priority -150; policy accept;
    meta l4proto tcp socket transparent 1 meta mark set 1 accept;
    meta l4proto udp socket transparent 1 meta mark set 1 accept;
    meta nfproto ipv4 udp dport 53 tproxy ip to 192.0.2.1:15053 meta mark set 1 accept;
    meta nfproto ipv6 udp dport 53 tproxy ip6 to [2001:db8::1]:15053 meta mark set 1 accept;
    ip daddr 127.0.0.0/8 return;
    ip daddr 192.0.2.1 return;
    ip6 daddr ::1 return;
    ip6 daddr 2001:db8::1 return;
    meta nfproto ipv4 meta l4proto tcp tproxy ip to 192.0.2.1:15001 meta mark set 1 accept;
    meta nfproto ipv6 meta l4proto tcp tproxy ip6 to [2001:db8::1]:15001 meta mark set 1 accept;
    meta nfproto ipv4 meta l4proto udp tproxy ip to 192.0.2.1:15053 meta mark set 1 accept;
    meta nfproto ipv6 meta l4proto udp tproxy ip6 to [2001:db8::1]:15053 meta mark set 1 accept;
    drop;
  }
}
EOF
# the same without `flags owner` — so a later command in the same namespace still finds it (the dumps)
sed '/flags owner;/d' /tmp/split.nft > /tmp/split-kept.nft
# cap NAME BEFORE COMMAND: in a fresh namespace, the setup and BEFORE untraced, then COMMAND traced — only the cases
# `ONLY` names, when it names any (the `create` cases were added after the first run and captured alone)
cap() {
  case " ${ONLY:-$1} " in *" $1 "*) ;; *) return 0 ;; esac
  printf '%s\n' "$2" > /tmp/before.sh
  echo; echo "=== $1"; echo "--- before: ${2:-(the setup only)}"; echo "--- traced: $3"
  unshare -Urn sh -c '
    { sh -e /tmp/setup.sh && sh -e /tmp/before.sh; } >/dev/null 2>&1 || echo "(the setup or what comes before failed)"
    strace -f -qq -e trace=sendto,sendmsg,recvfrom,recvmsg,setsockopt -e write=all -e read=all -o /tmp/st.txt sh -c "$1" 2>&1
    echo "--- exit $?"; echo "--- strace"; cat /tmp/st.txt' _ "$3"
}
B='nft -f /tmp/base.nft'
D='nft --debug=netlink'
R="$D add rule inet tarae"

# The rules split from r6's sets, each alone — in both chains where tarae has it in both
cap split-out-lo4 "$B" "$R out 'ip daddr 127.0.0.0/8 return'"
cap split-out-session4 "$B" "$R out 'ip daddr 192.0.2.1 return'"
cap split-out-lo6 "$B" "$R out 'ip6 daddr ::1 return'"
cap split-out-session6 "$B" "$R out 'ip6 daddr 2001:db8::1 return'"
cap split-out-tcp "$B" "$R out 'meta l4proto tcp meta mark set 1'"
cap split-out-udp "$B" "$R out 'meta l4proto udp meta mark set 1'"
cap split-out-dns "$B" "$R out 'udp dport 53 meta mark set 1 return'"
cap split-pre-socket-tcp "$B" "$R pre 'meta l4proto tcp socket transparent 1 meta mark set 1 accept'"
cap split-pre-socket-udp "$B" "$R pre 'meta l4proto udp socket transparent 1 meta mark set 1 accept'"
cap split-pre-lo4 "$B" "$R pre 'ip daddr 127.0.0.0/8 return'"
cap split-pre-session4 "$B" "$R pre 'ip daddr 192.0.2.1 return'"
cap split-pre-lo6 "$B" "$R pre 'ip6 daddr ::1 return'"
cap split-pre-session6 "$B" "$R pre 'ip6 daddr 2001:db8::1 return'"
cap pre-drop "$B" "$R pre drop"
# `socket transparent 0`, the rule only (whether it finds a session listener is measured elsewhere)
cap pre-socket-plain "$B" "$R pre 'meta l4proto tcp socket transparent 0 accept'"
# The whole ruleset, owned, as one batch — the golden for tarae's composition
cap split-batch '' "$D -f /tmp/split.nft"

# The kernel's errors with an extended ack: an attribute's offset (a rule for a chain that isn't there), the text
# (a prefix whose host bits are set — fib's "Invalid prefix for given prefix length"), and r6's EEXIST again with
# setsockopt traced (which options nft and ip set)
cap err-no-chain "$B" "nft add rule inet tarae nosuch accept"
cap err-route-prefix '' 'ip route add 192.0.2.1/8 dev lo'
cap err-exists-again "$B" 'nft create table inet tarae'

# The table and chains as tarae writes them — nft's `create` (EXCL: one already there is refused, never adopted):
# each alone, the whole ruleset as one batch of `create`s and `add rule`s (split.nft's rules, in its order), and the
# refusal of a chain that's there
cat > /tmp/create.nft <<'EOF'
create table inet tarae { flags owner; }
create chain inet tarae out { type route hook output priority -150; policy accept; }
create chain inet tarae pre { type filter hook prerouting priority -150; policy accept; }
EOF
awk '/^  chain / { c = $2; next } /^  \}/ { c = "" } c != "" && /^    / && !/^    type / {
  sub(/^ +/, ""); sub(/;$/, ""); print "add rule inet tarae " c " " $0 }' /tmp/split.nft >> /tmp/create.nft
case " ${ONLY:-create-batch} " in *" create-batch "*) echo; echo "--- /tmp/create.nft"; cat /tmp/create.nft ;; esac
OUT="type route hook output priority -150; policy accept;"
PRE="type filter hook prerouting priority -150; policy accept;"
cap create-table '' "$D create table inet tarae"
cap create-table-owner '' "$D create table inet tarae '{ flags owner; }'"
cap create-chain-route 'nft add table inet tarae' "$D create chain inet tarae out '{ $OUT }'"
cap create-chain-prerouting 'nft add table inet tarae' "$D create chain inet tarae pre '{ $PRE }'"
cap create-batch '' "$D -f /tmp/create.nft"
cap err-chain-exists "$B" "nft create chain inet tarae out '{ $OUT }'"

# The dumps tarae reads back (the live tests): the generation, the rules, the routes, the policy rules — after the
# whole ruleset and the routes are in
P='nft -f /tmp/split-kept.nft; ip route add default dev lo; ip -6 route add default dev lo; ip rule add fwmark 1 lookup 100; ip -6 rule add fwmark 1 lookup 100; ip route add local default dev lo table 100; ip -6 route add local default dev lo table 100'
cap dump-rules "$P" "$D list table inet tarae"
cap dump-routes4 "$P" 'ip route show table all'
cap dump-routes6 "$P" 'ip -6 route show table all'
cap dump-rules4 "$P" 'ip rule show'
cap dump-rules6 "$P" 'ip -6 rule show'
