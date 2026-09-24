FROM rust:1.96-alpine AS builder
RUN apk add --no-cache musl-dev cmake
WORKDIR /app
COPY . .
ARG VERSION=dev
RUN if [ "$VERSION" != "dev" ]; then grep -Fx "version = \"${VERSION#v}\"" Cargo.toml; fi
RUN cargo build --release --locked

FROM alpine:3.24
RUN apk add --no-cache ca-certificates tzdata ffmpeg && addgroup -S sirius && adduser -S -G sirius sirius
WORKDIR /app
COPY --from=builder /app/LICENSE* /app/NOTICE* /usr/share/licenses/sirius-asset-updater/
COPY --from=builder /app/target/release/sirius-asset-updater /usr/local/bin/sirius-asset-updater
ENV SIRIUS_ASSET_CONFIG_PATH=/app/sirius-asset-config.yaml
ARG VERSION=dev
LABEL org.opencontainers.image.version="${VERSION}"
RUN mkdir -p /app/downloads /app/exports && chown sirius:sirius /app/downloads /app/exports
USER sirius
ENTRYPOINT ["sirius-asset-updater"]
