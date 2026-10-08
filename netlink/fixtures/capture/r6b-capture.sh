#!/bin/sh
# R6b: r6's capture again, for what r6 left out — the ruleset without sets, the final drop, `socket transparent 0`,
# the extended-ack errors, `create`, the dumps tarae reads back. r6b-inner.sh on its stdin; writes $LOGS/r6b-<vm>.txt.
# With ONLY="case …" and TAG=name: only those cases, to $LOGS/r6b-<vm>-<name>.txt (the committed fixtures' `create`
# cases came so, TAG=create). Inside a time limit: nothing here should take a minute.
#   DOCKER_HOST=unix://… [ONLY="…" TAG=…] sh r6b-capture.sh <vm>
. "$(dirname "$0")/vm.sh"
{ printf "ONLY='%s'\n" "${ONLY:-}"; cat "$HERE/r6b-inner.sh"; } \
  | timeout 300 docker $RUN $IMAGE sh -s 2>&1 | tee "$LOGS/r6b-$VM${TAG:+-$TAG}.txt"
