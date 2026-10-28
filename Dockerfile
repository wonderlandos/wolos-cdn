FROM rust:1.96-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/wolos-cdn /usr/local/bin/wolos-cdn
VOLUME ["/data"]
ENV DATA_DIR=/data \
    ORIGIN=https://github.com/wonderlandos/wolos/releases/download \
    HOST=0.0.0.0 \
    PORT=80
EXPOSE 80
ENTRYPOINT ["wolos-cdn"]
