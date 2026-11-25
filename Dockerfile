FROM rust:1-slim-bookworm AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends libopus-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY benchmarks ./benchmarks
RUN cargo build --release --locked -p rustlink --bin rustlink -p rustlink-discord-bot --example music

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg libopus0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /app --no-create-home rustlink \
    && mkdir -p /app/.tools \
    && chown -R rustlink:rustlink /app
COPY --from=build /src/target/release/rustlink /usr/local/bin/rustlink
COPY --from=build /src/target/release/examples/music /usr/local/bin/rustlink-bot
COPY LICENSE NOTICE /usr/share/doc/rustlink/
ENV RUSTLINK_ADDRESS="0.0.0.0:2333" \
    RUSTLINK_FFMPEG="/usr/bin/ffmpeg" \
    RUSTLINK_TOOLS_DIRECTORY="/app/.tools" \
    RUSTLINK_AUTO_DOWNLOAD="false" \
    RUSTLINK_LOG_DIRECTORY=""
WORKDIR /app
USER rustlink
EXPOSE 2333
STOPSIGNAL SIGTERM
ENTRYPOINT ["rustlink"]
