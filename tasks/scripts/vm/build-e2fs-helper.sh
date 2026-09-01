#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build the minimal e2fs helper rootfs image for Windows libkrun VM.
#
# Creates an ext4 image containing Alpine Linux with e2fsprogs (mke2fs, debugfs)
# and a /init script that mounts /proc, /sys, /dev then exec's the command.
#
# Usage:
#   ./build-e2fs-helper.sh [output_dir]
#
# Output:
#   <output_dir>/e2fs-helper.ext4.zst
#
# Requirements:
#   - Docker (for building the Alpine rootfs)
#   - mke2fs (host, for creating the ext4 image)
#   - zstd (for compression)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/_lib.sh"
ROOT="$(vm_lib_root)"

OUTPUT_DIR="${1:-${OPENSHELL_VM_RUNTIME_COMPRESSED_DIR:-${ROOT}/target/vm-runtime-compressed}}"
mkdir -p "$OUTPUT_DIR"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

ROOTFS_DIR="${WORK_DIR}/rootfs"
IMAGE_FILE="${WORK_DIR}/e2fs-helper.ext4"
CONTAINER_NAME="openshell-e2fs-helper-build-$$"

echo "==> Building e2fs helper rootfs via Docker..."

docker create --name "$CONTAINER_NAME" alpine:3.20 /bin/true
docker start "$CONTAINER_NAME" || true
docker exec "$CONTAINER_NAME" apk add --no-cache e2fsprogs 2>/dev/null || \
    docker run --rm --name "${CONTAINER_NAME}-run" alpine:3.20 sh -c "apk add --no-cache e2fsprogs && sleep 1" || true

# Export the container filesystem
mkdir -p "$ROOTFS_DIR"
docker export "$CONTAINER_NAME" | tar -xf - -C "$ROOTFS_DIR" 2>/dev/null || {
    # If export fails (container never started), use docker run + export
    docker rm -f "$CONTAINER_NAME" 2>/dev/null || true
    docker run --name "$CONTAINER_NAME" alpine:3.20 sh -c "apk add --no-cache e2fsprogs"
    docker export "$CONTAINER_NAME" | tar -xf - -C "$ROOTFS_DIR"
}
docker rm -f "$CONTAINER_NAME" 2>/dev/null || true

echo "==> Writing /init script..."

cat > "${ROOTFS_DIR}/init" << 'EOF'
#!/bin/sh
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mkdir -p /mnt/source /mnt/inject
exec "$@"
EOF
chmod 755 "${ROOTFS_DIR}/init"

echo "==> Creating ext4 image..."

# Calculate size: rootfs + 16 MiB headroom, minimum 64 MiB
ROOTFS_SIZE=$(du -sb "$ROOTFS_DIR" | awk '{print $1}')
IMAGE_SIZE=$(( (ROOTFS_SIZE + 16*1024*1024 + 1048575) / 1048576 * 1048576 ))
if [ "$IMAGE_SIZE" -lt 67108864 ]; then
    IMAGE_SIZE=67108864
fi

truncate -s "$IMAGE_SIZE" "$IMAGE_FILE"
mke2fs -q -F -t ext4 -E root_owner=0:0 -d "$ROOTFS_DIR" "$IMAGE_FILE"

echo "==> Compressing with zstd..."

zstd -19 --rm -f "$IMAGE_FILE" -o "${OUTPUT_DIR}/e2fs-helper.ext4.zst"

FINAL_SIZE=$(stat -c%s "${OUTPUT_DIR}/e2fs-helper.ext4.zst" 2>/dev/null || stat -f%z "${OUTPUT_DIR}/e2fs-helper.ext4.zst")
echo "==> Done: ${OUTPUT_DIR}/e2fs-helper.ext4.zst (${FINAL_SIZE} bytes)"
