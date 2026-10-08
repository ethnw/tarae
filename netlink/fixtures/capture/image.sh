#!/bin/sh
# R0: the image the captures run on, made on the VM's Docker — rust:1.99 (as xtask's) with nftables, iproute2, kmod,
# strace — then one throwaway container saying the tools' versions ($LOGS/r0-<vm>.txt). Nothing is left running.
#   DOCKER_HOST=unix://… sh image.sh <vm>
. "$(dirname "$0")/vm.sh"
printf 'FROM rust:1.99\nRUN apt-get update -qq && apt-get install -y -qq --no-install-recommends nftables iproute2 kmod strace >/dev/null && rm -rf /var/lib/apt/lists/*\n' \
  | docker build -q -t $IMAGE - || exit 1
docker $RUN $IMAGE sh -c 'nft --version; ip -V; unshare --version; rustc --version; id' 2>&1 | tee "$LOGS/r0-$VM.txt"
