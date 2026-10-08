# Sourced by the captures: a Linux VM's Docker (DOCKER_HOST, a socket forwarded from it; $1 names the VM in the logs)
# and `cargo xtask linux --userns`'s container options — SYS_PTRACE, the container's seccomp, AppArmor and masked paths
# lifted — run as uid 1000 with no capabilities of its own, on image.sh's image. Logs go to $LOGS. In order:
# image.sh, r6-capture.sh, r6b-capture.sh, then fixtures.sh (which needs no VM).
[ -n "${1:-}" ] && [ -n "${DOCKER_HOST:-}" ] || { echo "usage: DOCKER_HOST=unix://… sh $0 <vm: deb12 …>"; exit 2; }
# (a Docker config of its own, as the captures were made with — not the user's ~/.docker)
export DOCKER_CONFIG=/tmp/tarae-dc
VM=$1
HERE=$(cd "$(dirname "$0")" && pwd)
LOGS=${LOGS:-$(cd "$HERE/../../.." && pwd)/target/sb/redirect}
mkdir -p "$LOGS"
IMAGE=tarae-redirect
NAME=tarae-redirect-$$
# (a container interrupted with the script goes too: --rm covers the normal end)
trap 'docker rm -f $NAME >/dev/null 2>&1' EXIT INT TERM
RUN="run --rm -i --name $NAME --user 1000:1000 --cap-add SYS_PTRACE --security-opt seccomp=unconfined
  --security-opt apparmor=unconfined --security-opt systempaths=unconfined
  -e PATH=/usr/local/cargo/bin:/usr/sbin:/usr/bin:/sbin:/bin -e HOME=/tmp"
