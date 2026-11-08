# syntax=docker/dockerfile:1
# OwnHealth: one app image (the Rust server with the built UI inside, MCP, job workers). Postgres
# runs beside it (compose.yaml).

# 1. UI: SvelteKit static build, written into server/static (vite.config.ts).
FROM node:24.22.2-bookworm-slim AS ui
WORKDIR /src/ui
RUN corepack enable
COPY ui/package.json ui/pnpm-lock.yaml ui/.npmrc ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN mkdir -p ../server/static && pnpm build

# 2. Server: the release binary with the UI embedded (rust-embed reads server/static at build time),
#    and the pinned PDFium build for this architecture (scripts/fetch-pdfium.sh checks its sha256).
#    The data files (migrations/, and data/: rule pack, CPIC, LOINC subset, vendor templates, genome
#    schema) are compiled in.
FROM rust:1.98.1-slim-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates cmake curl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY scripts/fetch-pdfium.sh scripts/
ARG TARGETARCH
RUN case "$TARGETARCH" in amd64) p=linux-x64 ;; arm64) p=linux-arm64 ;; *) echo "no PDFium build for $TARGETARCH" >&2; exit 1 ;; esac \
 && bash scripts/fetch-pdfium.sh "$p" \
 && mkdir -p /out \
 && cp "server/vendor/pdfium/$p/lib/libpdfium.so" /out/libpdfium.so \
 && cp "server/vendor/pdfium/$p/LICENSE" /out/pdfium-LICENSE
COPY server/ server/
COPY contract-tests/ contract-tests/
COPY migrations/ migrations/
COPY data/ data/
COPY --from=ui /src/server/static server/static
WORKDIR /src/server
RUN --mount=type=cache,id=ownhealth-cargo-registry-$TARGETARCH,target=/usr/local/cargo/registry \
    --mount=type=cache,id=ownhealth-cargo-target-$TARGETARCH,target=/src/server/target \
    cargo build --release --locked -p ownhealth \
 && cp target/release/ownhealth /out/ownhealth \
 && /out/ownhealth --version

# 3. Runtime: slim, non-root, no build tools.
FROM debian:trixie-20261104-slim
LABEL org.opencontainers.image.title="OwnHealth" \
      org.opencontainers.image.source="https://github.com/GeiserX/OwnHealth" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"
# pg_dump/pg_restore/psql 17 and restic for `ownhealth backup | restore | verify-restore`
# (deploy/restic/) and the admin service's app-role.sh.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates postgresql-client-17 restic \
 && rm -rf /var/lib/apt/lists/*
RUN groupadd --system --gid 10001 ownhealth \
 && useradd --system --uid 10001 --gid ownhealth --home-dir /data --shell /usr/sbin/nologin ownhealth \
 && mkdir -p /data && chown ownhealth:ownhealth /data
COPY --from=build /out/ownhealth /usr/local/bin/ownhealth
COPY --from=build /out/libpdfium.so /usr/local/lib/libpdfium.so
COPY --from=build /out/pdfium-LICENSE /usr/share/doc/pdfium/LICENSE
RUN ldconfig
# The admin service runs these after migrating; baked in so a stack needs no checkout.
COPY deploy/db/ /opt/ownhealth-db/
ENV OWNHEALTH_DATA_DIR=/data \
    OWNHEALTH_HOST=0.0.0.0 \
    OWNHEALTH_PORT=8080 \
    OWNHEALTH_PDFIUM_LIB=/usr/local/lib/libpdfium.so
USER ownhealth
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["ownhealth", "healthcheck"]
CMD ["ownhealth", "serve"]
