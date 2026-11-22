# memdusa server image:  docker run -p 7077:7077 -v memdusa:/data ghcr.io/<owner>/memdusa
FROM rust:1-bookworm AS build
WORKDIR /src
# Build dependencies first so source edits don't rebuild them.
COPY Cargo.toml Cargo.lock ./
COPY crates/memdusa-core/Cargo.toml crates/memdusa-core/
COPY crates/memdusa/Cargo.toml crates/memdusa/
RUN mkdir -p crates/memdusa-core/src crates/memdusa/src \
 && echo "" > crates/memdusa-core/src/lib.rs && echo "fn main() {}" > crates/memdusa/src/main.rs \
 && cargo build --release --locked -p memdusa \
 && rm -rf crates/*/src
COPY crates crates
RUN touch crates/memdusa-core/src/lib.rs crates/memdusa/src/main.rs \
 && cargo build --release --locked -p memdusa

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data memdusa \
 && mkdir /data && chown memdusa /data
COPY --from=build /src/target/release/memdusa /usr/local/bin/memdusa
COPY LICENSE NOTICE /usr/share/doc/memdusa/
USER memdusa
ENV MEMDUSA_HOME=/data \
    MEMDUSA_DATABASE_URL=sqlite:///data/memdusa.db \
    MEMDUSA_ADDR=0.0.0.0:7077
VOLUME /data
EXPOSE 7077
HEALTHCHECK --interval=30s --timeout=3s CMD ["memdusa", "--version"]
ENTRYPOINT ["memdusa"]
CMD ["serve"]
