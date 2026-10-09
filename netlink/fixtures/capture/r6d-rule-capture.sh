#!/bin/sh
# R6d's policy rule: ip's bytes for `fwmark 0x1/0x1 lookup 100`, both families — bit 0 the output chain adds to every
# mark (6d), the run's own bits beside it (M12's rule). Writes $LOGS/r6b-<vm>-6d-rule.txt. Usage as r6g-capture.sh.
. "$(dirname "$0")/vm.sh"; { echo "ONLY=none"; cat "$HERE/r6b-inner.sh"; cat <<'EOF'
ONLY=; cap rule-fwmark4-bit '' 'ip rule add fwmark 0x1/0x1 lookup 100'
cap rule-fwmark6-bit '' 'ip -6 rule add fwmark 0x1/0x1 lookup 100'
EOF
} | timeout 300 docker $RUN $IMAGE sh -s 2>&1 | tee "$LOGS/r6b-$VM-6d-rule.txt"
