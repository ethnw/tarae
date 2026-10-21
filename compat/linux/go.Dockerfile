# compat's Go image: Go from go.dev and golangci-lint from its release, each sha256 pinned here (golangci-lint 2's
# own Go reads 1.27's standard library). GOTOOLCHAIN=local: the pre-warm never fetches another toolchain.
# Built by `cargo xtask compat` on rust:<pin> (BASE); the tag is the recipe's hash.
ARG BASE
FROM ${BASE}
ARG GO=1.27.2
ARG LINT=2.14.0
RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
      arm64) a=arm64; s=94f3e30b8e374bc285e7dadc11e0865726b9bc6e85b841ccceaabc0214c6b7c8; \
             l=ee7ec5f3453d15ddf106fae5a4d6c71737712348a979d1fe9cd52ec7ea299bae ;; \
      amd64) a=amd64; s=ecbadb99091a3f46e31f5f934b068b1864eafa7995211b39eaddf76996045fe5; \
             l=ab90aeb7b066f92a33415b638a50fe5344bbb75a0d32ad30cc248d88f81032ab ;; \
      *) echo "no Go for $(dpkg --print-architecture)"; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/go.tar.gz "https://dl.google.com/go/go$GO.linux-$a.tar.gz"; \
    echo "$s  /tmp/go.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/go.tar.gz -C /usr/local --no-same-owner; \
    curl -fsSLo /tmp/lint.tar.gz \
      "https://github.com/golangci/golangci-lint/releases/download/v$LINT/golangci-lint-$LINT-linux-$a.tar.gz"; \
    echo "$l  /tmp/lint.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/lint.tar.gz -C /usr/local/bin --strip-components=1 --no-same-owner \
      "golangci-lint-$LINT-linux-$a/golangci-lint"; \
    rm /tmp/go.tar.gz /tmp/lint.tar.gz; \
    /usr/local/go/bin/go version; golangci-lint --version
ENV PATH=/usr/local/go/bin:$PATH GOTOOLCHAIN=local
