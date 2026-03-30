#!/bin/bash
# Test desktopinator's DRM backend in a QEMU VM with virtio-gpu.
#
# Prerequisites: qemu-system-x86_64, qemu-utils, cloud-image-utils
#
# Usage:
#   ./test-vm.sh create    # Download cloud image + create VM disk
#   ./test-vm.sh deploy    # Copy desktopinator binary into the VM
#   ./test-vm.sh run       # Boot the VM (GTK window with GPU)
#   ./test-vm.sh ssh       # SSH into the running VM
#
# Inside the VM:
#   desktopinator --drm    # Run as the bare-metal compositor
#   foot                   # Launch a terminal (from another VT)

set -euo pipefail

VM_DIR="$(dirname "$0")/.test-vm"
IMG="$VM_DIR/disk.qcow2"
BASE="$VM_DIR/debian.qcow2"
CIDATA="$VM_DIR/cidata.iso"
SSH_PORT=2222

create() {
    mkdir -p "$VM_DIR"

    # Download base image if needed
    if [ ! -f "$BASE" ]; then
        echo "Downloading Debian trixie cloud image..."
        curl -fsSL -o "$BASE" \
            "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2"
    fi

    # Create VM disk as overlay on base
    echo "Creating VM disk..."
    cp "$BASE" "$IMG"
    qemu-img resize "$IMG" 8G

    # Create cloud-init ISO
    echo "Creating cloud-init config..."
    if command -v cloud-localds &>/dev/null; then
        cloud-localds "$CIDATA" "$VM_DIR/user-data" "$VM_DIR/meta-data"
    elif command -v genisoimage &>/dev/null; then
        genisoimage -output "$CIDATA" -volid cidata -joliet -rock \
            "$VM_DIR/user-data" "$VM_DIR/meta-data"
    else
        echo "Need cloud-image-utils or genisoimage for cloud-init"
        echo "  sudo apt install cloud-image-utils"
        exit 1
    fi

    echo "VM created. Run: ./test-vm.sh run"
}

deploy() {
    echo "Deploying desktopinator to VM via SSH..."
    scp -P $SSH_PORT -o StrictHostKeyChecking=no \
        target/release/desktopinator target/release/dinatorctl \
        test@localhost:/tmp/
    ssh -p $SSH_PORT -o StrictHostKeyChecking=no test@localhost \
        "sudo cp /tmp/desktopinator /tmp/dinatorctl /usr/local/bin/ && sudo chmod +x /usr/local/bin/desktopinator /usr/local/bin/dinatorctl"
    echo "Deployed."
}

run() {
    echo "Booting VM... (window will open with GPU display)"
    echo "  SSH: ssh -p $SSH_PORT test@localhost (password: test)"
    echo "  Ctrl-Alt-F to toggle fullscreen"

    local CIDATA_ARG=""
    if [ -f "$CIDATA" ]; then
        CIDATA_ARG="-drive file=$CIDATA,format=raw,if=virtio"
    fi

    qemu-system-x86_64 \
        -enable-kvm \
        -m 2G \
        -smp 2 \
        -drive file="$IMG",format=qcow2,if=virtio \
        $CIDATA_ARG \
        -device virtio-vga-gl \
        -device virtio-keyboard-pci \
        -device virtio-mouse-pci \
        -display gtk,gl=on \
        -nic user,hostfwd=tcp::${SSH_PORT}-:22 \
        -serial mon:stdio
}

ssh_cmd() {
    ssh -p $SSH_PORT -o StrictHostKeyChecking=no test@localhost
}

case "${1:-help}" in
    create) create ;;
    deploy) deploy ;;
    run) run ;;
    ssh) ssh_cmd ;;
    *)
        echo "Usage: $0 {create|deploy|run|ssh}"
        echo "  create  - Download Debian cloud image + create VM"
        echo "  deploy  - Copy desktopinator binaries via SSH"
        echo "  run     - Boot VM with virtio-gpu (GTK window)"
        echo "  ssh     - SSH into running VM"
        ;;
esac
