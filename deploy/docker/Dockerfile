# syntax=docker/dockerfile:1.7
ARG MATTER_TAG=v1.5.1.0
ARG NINJA_JOBS=4

FROM rust:1.99-slim-bookworm AS heatd-build
RUN apt-get update && apt-get install -y --no-install-recommends g++ pkg-config libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/root/.cache/ort.pyke.io \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p heatd && cp target/release/heatd /heatd

FROM ubuntu:24.04 AS paa
ARG MATTER_TAG
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/*
RUN git clone --quiet --depth 1 --branch ${MATTER_TAG} --filter=blob:none --sparse https://github.com/project-chip/connectedhomeip.git /repo \
    && git -C /repo sparse-checkout set credentials/production/paa-root-certs \
    && mkdir -p /paa-root-certs && cp /repo/credentials/production/paa-root-certs/* /paa-root-certs/ \
    && ls /paa-root-certs | wc -l

FROM ubuntu:24.04 AS chip-tool-build
ARG MATTER_TAG
ARG NINJA_JOBS
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    git ca-certificates curl unzip xz-utils gcc g++ pkg-config ninja-build python3 python3-venv python3-dev python3-pip \
    libssl-dev libdbus-1-dev libglib2.0-dev libavahi-client-dev libgirepository1.0-dev libcairo2-dev libreadline-dev libevent-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /connectedhomeip
RUN git clone --quiet --depth 1 --branch ${MATTER_TAG} https://github.com/project-chip/connectedhomeip.git . \
    && python3 scripts/checkout_submodules.py --allow-changing-global-git-config --shallow --platform linux
SHELL ["/bin/bash", "-c"]
RUN bash scripts/bootstrap.sh
RUN mkdir -p /shim && printf '#!/bin/sh\nexec %s -j %s "$@"\n' "$(which ninja)" "${NINJA_JOBS}" > /shim/ninja && chmod +x /shim/ninja \
    && source scripts/activate.sh && export PATH=/shim:$PATH && scripts/build/build_examples.py --target linux-x64-chip-tool build \
    && strip -o /chip-tool out/linux-x64-chip-tool/chip-tool

FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl libssl3t64 libglib2.0-0t64 libdbus-1-3 libstdc++6 libreadline8t64 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=heatd-build /heatd /usr/local/bin/heatd
COPY --from=chip-tool-build /chip-tool /usr/local/bin/chip-tool
COPY --from=paa /paa-root-certs /opt/paa-root-certs
RUN mkdir -p /data /models /chip-storage /secrets && chown 1000:1000 /data /models /chip-storage /secrets
RUN ! ldd /usr/local/bin/chip-tool /usr/local/bin/heatd | grep "not found"
ENV HEATD_CONFIG=/etc/heatd/heatd.toml
USER 1000:1000
WORKDIR /data
ENTRYPOINT ["heatd"]
CMD ["serve"]
