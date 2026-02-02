FROM debian:bookworm-slim

LABEL maintainer="shenjindi@miuda.ai"
LABEL org.opencontainers.image.source="https://github.com/restsend/rustpbx"
LABEL org.opencontainers.image.description="A SIP PBX implementation in Rust"

# Set environment variables
ARG DEBIAN_FRONTEND=noninteractive
ENV LANG=C.UTF-8
ENV TZ=UTC

# Install runtime dependencies
RUN --mount=type=cache,target=/var/cache/apt \
    apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tzdata \
    libopus0 \
    && rm -rf /var/lib/apt/lists/*

# Create application directory structure
WORKDIR /app
RUN mkdir -p /app/config /app/sounds /app/templates

# Automatically pick the correct binary based on the architecture being built
# We expect binaries to be placed in bin/amd64/ and bin/arm64/ by the build script
ARG TARGETARCH
COPY bin/${TARGETARCH}/rustpbx /app/rustpbx
COPY bin/${TARGETARCH}/sipflow /app/sipflow

# Copy static resources
COPY ./static /app/static
COPY ./templates /app/templates
COPY ./config/sounds /app/sounds

# Copy addon static and templates
COPY ./src/addons/acme/static /app/static/acme
COPY ./src/addons/transcript/static /app/static/transcript
COPY ./src/addons/queue/static /app/static/queue
COPY ./src/addons/acme/templates /app/templates/acme
COPY ./src/addons/archive/templates /app/templates/archive
COPY ./src/addons/queue/templates /app/templates/queue
COPY ./src/addons/transcript/templates /app/templates/transcript

ENTRYPOINT ["/app/rustpbx"]
