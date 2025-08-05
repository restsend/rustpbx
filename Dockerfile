FROM rust:bookworm AS rust-builder
RUN apt-get update && apt-get install -y libasound2-dev libopus-dev
RUN mkdir /build
ADD . /build/
WORKDIR /build
RUN --mount=type=cache,target=/build/.cargo/registry \
    --mount=type=cache,target=/build/target/release/incremental\
    --mount=type=cache,target=/build/target/release/build\
    cargo build --release --bin rustpbx

FROM debian:bookworm
LABEL maintainer="shenjindi@fourz.cn"
RUN --mount=type=cache,target=/var/apt apt-get update && apt-get install -y ca-certificates tzdata libopus0
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8

WORKDIR /app
COPY --from=rust-builder /build/target/release/rustpbx /app/rustpbx

EXPOSE 25010/UDP
EXPOSE 5060/UDP
EXPOSE 8080/TCP
EXPOSE 12000-52000/UDP

ENTRYPOINT ["/app/rustpbx"]