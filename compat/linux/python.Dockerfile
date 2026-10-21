# compat's Python image (python-pip, python-uv): Debian's python3 with venv, and uv from its release, the sha256
# pinned here. The standard library's bytecode is deleted, as a Python just installed or upgraded has none: a
# module first imported in the sandbox is then refused there (§9's B1, which macOS can't show twice).
# Built by `cargo xtask compat` on rust:<pin> (BASE); the tag is the recipe's hash.
ARG BASE
FROM ${BASE}
ARG UV=0.13.1
RUN set -eu; \
    apt-get update -qq; \
    apt-get install -y -qq --no-install-recommends python3-venv >/dev/null; \
    rm -rf /var/lib/apt/lists/*; \
    case "$(dpkg --print-architecture)" in \
      arm64) a=aarch64; s=9041c73c99fdc5892cf128140beb354eca9231d502068cc36e6cb53a8e59ad14 ;; \
      amd64) a=x86_64; s=eadd9d27c134ab5ac1ffd4473b97a741407f3ea238288f03121fc4deddf90220 ;; \
      *) echo "no uv for $(dpkg --print-architecture)"; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/uv.tar.gz "https://github.com/astral-sh/uv/releases/download/$UV/uv-$a-unknown-linux-gnu.tar.gz"; \
    echo "$s  /tmp/uv.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/uv.tar.gz -C /usr/local/bin --strip-components=1 --no-same-owner \
      "uv-$a-unknown-linux-gnu/uv" "uv-$a-unknown-linux-gnu/uvx"; \
    rm /tmp/uv.tar.gz; \
    find /usr/lib/python3* -name __pycache__ -type d -prune -exec rm -rf {} +; \
    python3 --version; uv --version
