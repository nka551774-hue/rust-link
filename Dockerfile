# The build stage carries the headers the Opus encoder is linked against and
# compiles the whole workspace, which the binary crate depends on.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends libopus-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY benchmarks ./benchmarks
RUN cargo build --release --locked --bin rustlink

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg libopus0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home rustlink \
    && mkdir -p /app/.tools \
    && chown -R rustlink:rustlink /app
COPY --from=build /src/target/release/rustlink /usr/local/bin/rustlink
COPY LICENSE NOTICE /usr/share/doc/rustlink/
# FFmpeg comes from the distribution and is pinned by path, because a bare name
# only ever means a file inside the tools directory. The container already has
# its codec helper, so startup does not need any downloads.
ENV RUSTLINK_ADDRESS="0.0.0.0:2333" \
    RUSTLINK_FFMPEG="/usr/bin/ffmpeg" \
    RUSTLINK_TOOLS_DIRECTORY="/app/.tools" \
    RUSTLINK_AUTO_DOWNLOAD="false"
WORKDIR /app
USER rustlink
EXPOSE 2333
STOPSIGNAL SIGTERM
ENTRYPOINT ["rustlink"]
