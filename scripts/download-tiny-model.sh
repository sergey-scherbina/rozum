#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
MODEL_DIR="${MODEL_DIR:-$ROOT/models}"
REPO="${ROZUM_TINY_REPO:-bartowski/SmolLM2-135M-Instruct-GGUF}"
FILE="${ROZUM_TINY_FILE:-SmolLM2-135M-Instruct-Q4_K_M.gguf}"
URL="https://huggingface.co/$REPO/resolve/main/$FILE"
TARGET="$MODEL_DIR/$FILE"

mkdir -p "$MODEL_DIR"
curl --fail --location --continue-at - "$URL" --output "$TARGET"
printf '%s\n' "$TARGET"
