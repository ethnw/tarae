#!/bin/sh
# R6g: nft's bytes for 6g's `socket cgroupv2` rule — r6b-inner.sh with its own cases skipped (ONLY) and one case after
# them. In the host's cgroup namespace (`--cgroupns host`: nft names a cgroup by its path, and a container's own
# namespace has no folder below its root), mounted read-only as Docker mounts it; nft writes the folder's inode as the
# id — said in the case's command, the only value of the bytes that's the machine's. Writes $LOGS/r6b-<vm>-cgroup.txt.
#   DOCKER_HOST=unix://… sh r6g-capture.sh <vm>
. "$(dirname "$0")/vm.sh"
{
  echo "ONLY='split-out-cgroup'"
  cat "$HERE/r6b-inner.sh"
  cat <<'EOF'
I=$(stat -c %i /sys/fs/cgroup/system.slice)
cap split-out-cgroup "$B" "$R out 'socket cgroupv2 level 1 \"system.slice\" meta mark set 0x11 accept' # system.slice is inode $I"
EOF
} | timeout 300 docker $RUN --cgroupns host $IMAGE sh -s 2>&1 | tee "$LOGS/r6b-$VM-cgroup.txt"
