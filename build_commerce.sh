#!/usr/bin/env bash
set -euo pipefail

# ============================================================
# build_cn.sh - Build rustpbx commerce edition and push
# ============================================================
# Usage:
#   ./build_cn.sh                    # use defaults
#   GA_ID=G-XXXXXXXXXX ./build_cn.sh # override GA tracking ID
#   TAG=v0.4.13 ./build_cn.sh        # custom image tag
# ============================================================

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd -P)"
cd "$SCRIPT_DIR"

# --- Configuration ---
GA_ID="${GA_ID:-G-BBGHM1J5Q9}"
TAG="${TAG:-latest}"
IMAGE="docker.cnb.cool/miuda.ai/rustpbx:${TAG}"
ARCH="${ARCH:-$(uname -m)}"

# Normalize arch for Docker TARGETARCH
case "$ARCH" in
    x86_64)  DOCKER_ARCH="amd64" ;;
    aarch64) DOCKER_ARCH="arm64" ;;
    *)       DOCKER_ARCH="$ARCH" ;;
esac

echo "=========================================="
echo " Build rustpbx Commerce Edition"
echo "=========================================="
echo "  ARCH:       $ARCH (docker: $DOCKER_ARCH)"
echo "  GA_ID:      $GA_ID"
echo "  Image:      $IMAGE"
echo "=========================================="

# --- Step 1: Update submodules ---
echo ""
echo "[1/4] Updating git submodules..."
git pull
git submodule update --init --recursive

# --- Check for clang (required for libsqlite3-sys bundled build) ---
if ! command -v clang &>/dev/null; then
    echo "[*] Installing clang..."
    sudo apt-get install -y clang
fi
export CC=clang

# --- Step 2: Cargo build with all commercial features ---
echo ""
echo "[2/4] Building with cargo (features: default,commerce,wholesale,contact-center)..."
RUST_MIN_STACK=268435456 cargo build --release \
    --features default,commerce,wholesale,contact-center

# --- Step 3: Prepare binaries for Docker ---
echo ""
echo "[3/4] Preparing binaries..."
mkdir -p "bin/${DOCKER_ARCH}"
cp target/release/rustpbx "bin/${DOCKER_ARCH}/rustpbx"
cp target/release/sipflow "bin/${DOCKER_ARCH}/sipflow"
chmod +x "bin/${DOCKER_ARCH}/rustpbx" "bin/${DOCKER_ARCH}/sipflow"

# --- Step 4: Docker build & push ---
echo ""
echo "[4/4] Building Docker image..."
docker build \
    --platform "linux/${DOCKER_ARCH}" \
    --build-arg GA_ID="${GA_ID}" \
    -f Dockerfile.commerce \
    -t "$IMAGE" \
    .

echo ""
echo "Pushing $IMAGE ..."
docker push "$IMAGE"

echo ""
echo "=========================================="
echo " Done! Image: $IMAGE"
echo "=========================================="
