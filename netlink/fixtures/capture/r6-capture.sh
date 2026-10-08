#!/bin/sh
# R6: the netlink messages nft and ip send for the redirect's rules and routes, and the kernel's answers — r6-inner.sh
# on its stdin, each case in a namespace of its own. Writes $LOGS/r6-<vm>.txt.
#   DOCKER_HOST=unix://… sh r6-capture.sh <vm>
. "$(dirname "$0")/vm.sh"
docker $RUN $IMAGE sh -s < "$HERE/r6-inner.sh" 2>&1 | tee "$LOGS/r6-$VM.txt"
