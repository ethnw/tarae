# netlink.txt from the strace logs r6-inner.sh and r6b-inner.sh wrote (fixtures.sh names the cases): only what the
# tests read. For each case kept: its command, the last datagram it sent (`>` — the batch or change the crate's bytes are
# held against) where SENT names it, and every datagram the kernel answered (`<`) where RECV names it; each datagram
# hex, 64 bytes a line, `  ` before a continuation.
#   awk -v SENT=a,b -v RECV=c,d -f fixtures.awk r6.txt r6b.txt
BEGIN {
  n = split(SENT, s, ","); for (i = 1; i <= n; i++) sent[s[i]] = 1
  n = split(RECV, r, ","); for (i = 1; i <= n; i++) recv[r[i]] = 1
  print "# tarae-netlink's fixtures: what nft 1.1.3 and iproute2 6.15 sent the kernel (Debian 12, Linux 6.1, each case in"
  print "# a user + network namespace of its own) and what it answered — from strace's logs of capture/r6-inner.sh and"
  print "# r6b-inner.sh, kept by capture/fixtures.sh (how to make them again: capture/vm.sh). Per case: the command, the last"
  print "# datagram sent (>), the kernel's answers (<), each datagram hex, 64 bytes a line, two spaces before a continuation."
}
function hexout(mark, h,    i) {
  for (i = 1; i <= length(h); i += 128) print (i == 1 ? mark " " : "  ") substr(h, i, 128)
}
function endcall() {
  if (open && buf != "") { if (kind == ">") last = buf; else got[++ngot] = buf }
  open = 0; buf = ""
}
function flush(    i) {
  endcall()
  if (cur == "" || !((cur in sent) || (cur in recv))) return
  print ""; print "=== " cur; print "# " cmd
  if ((cur in sent) && last != "") hexout(">", last)
  if (cur in recv) for (i = 1; i <= ngot; i++) hexout("<", got[i])
}
/^=== / { flush(); cur = $2; cmd = ""; last = ""; ngot = 0; next }
/^--- traced: / { cmd = substr($0, 13); next }
/^ \| / { if (open) { h = substr($0, 11, 48); gsub(/ /, "", h); buf = buf h }; next }
/^ \* / { next }
{
  endcall()
  line = $0; sub(/^[0-9]+ +/, "", line)
  if (line ~ /^(sendto|sendmsg)\(/) { open = 1; kind = ">" }
  else if (line ~ /^(recvmsg|recvfrom)\(/) { open = 1; kind = "<" }
}
END { flush() }
