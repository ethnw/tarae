# compat's Node image (node-npm, node-pnpm): Node 24 LTS from nodejs.org, its sha256 pinned here, and corepack's
# shims for pnpm (Node 24 still bundles corepack; 25 doesn't). pnpm itself is corepack's download, into
# ~/.cache/node/corepack on the stack's volume, at the pre-warm — where a user's corepack keeps it.
# Built by `cargo xtask compat` on rust:<pin> (BASE); the tag is the recipe's hash.
ARG BASE
FROM ${BASE}
ARG NODE=24.22.0
# (before the RUN: corepack is `#!/usr/bin/env node`)
ENV PATH=/usr/local/node/bin:$PATH COREPACK_ENABLE_DOWNLOAD_PROMPT=0
RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
      arm64) a=arm64; s=fef00bb4d36e1c50104a9775a10359beee98f162b4104dd12472c75065c6a143 ;; \
      amd64) a=x64; s=cc2f601cee82d9a5a233302fe307abc600727035fd7d2c3a3571755989e95008 ;; \
      *) echo "no Node for $(dpkg --print-architecture)"; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/node.tar.xz "https://nodejs.org/dist/v$NODE/node-v$NODE-linux-$a.tar.xz"; \
    echo "$s  /tmp/node.tar.xz" | sha256sum -c -; \
    mkdir /usr/local/node; \
    tar -xJf /tmp/node.tar.xz -C /usr/local/node --strip-components=1 --no-same-owner; \
    rm /tmp/node.tar.xz; \
    /usr/local/node/bin/corepack enable --install-directory /usr/local/node/bin pnpm; \
    node --version
