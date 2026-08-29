#!/bin/bash
# Setup script for patched winit (AquaSKK compatibility fix)
#
# This script copies winit 0.30.13 from the Cargo cache (or downloads it)
# and applies the AquaSKK compatibility patch. Run this after cloning
# the repository.
#
# Usage:
#   ./scripts/setup-winit.sh
#
# The patched winit will be placed in patches/winit/ and referenced
# via [patch.crates-io] in Cargo.toml.

set -euo pipefail

WINIT_VERSION="0.30.13"
PATCH_FILE="patches/winit-aquaskk.patch"
TARGET_DIR="patches/winit"

# Skip if already set up
if [ -d "${TARGET_DIR}/.git" ]; then
    echo "winit is already set up at ${TARGET_DIR}"
    exit 0
fi

# Check patch file exists
if [ ! -f "${PATCH_FILE}" ]; then
    echo "ERROR: Patch file not found: ${PATCH_FILE}" >&2
    exit 1
fi

# Find winit in Cargo cache
find_winit_cache() {
    local cache_dir
    for cache_dir in ~/.cargo/registry/src/*/; do
        if [ -d "${cache_dir}winit-${WINIT_VERSION}" ]; then
            echo "${cache_dir}winit-${WINIT_VERSION}"
            return 0
        fi
    done
    return 1
}

# Download winit using cargo
download_winit() {
    echo "winit not found in Cargo cache. Downloading..."

    # Create a temporary project to download the dependency
    local tmp_dir
    tmp_dir=$(mktemp -d)
    trap "rm -rf ${tmp_dir}" EXIT

    cd "${tmp_dir}"
    cat > Cargo.toml <<EOF
[package]
name = "winit-download"
version = "0.1.0"
edition = "2021"

[dependencies]
winit = "${WINIT_VERSION}"
EOF
    mkdir -p src
    echo 'fn main() {}' > src/main.rs

    cargo fetch 2>/dev/null || cargo build 2>/dev/null || true

    # Try to find it in cache now
    local cache_path
    cache_path=$(find_winit_cache) || {
        echo "ERROR: Failed to download winit ${WINIT_VERSION}" >&2
        echo "Try running: cargo add winit@${WINIT_VERSION} in a temporary project" >&2
        return 1
    }

    echo "${cache_path}"
}

echo "Looking for winit ${WINIT_VERSION}..."

WINIT_SRC=$(find_winit_cache) || WINIT_SRC=$(download_winit)

if [ -z "${WINIT_SRC}" ] || [ ! -d "${WINIT_SRC}" ]; then
    echo "ERROR: Could not find or download winit ${WINIT_VERSION}" >&2
    exit 1
fi

echo "Found: ${WINIT_SRC}"

# Copy to target
cp -R "${WINIT_SRC}" "${TARGET_DIR}"

echo "Initializing git repository..."
cd "${TARGET_DIR}"
git init
git add -A
git commit -m "Import winit ${WINIT_VERSION} (original from crates.io)" --quiet

echo "Applying AquaSKK patch..."
git apply "../../${PATCH_FILE}"
git add -A
git commit -m "fix(macos): AquaSKK q/l key handling fix" --quiet

cd ../..

echo ""
echo "Success! Patched winit is ready at ${TARGET_DIR}/"
echo "You can now build with: make app"
