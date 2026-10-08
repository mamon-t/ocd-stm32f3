#!/bin/bash
set -e

echo "Building firmware..."
cargo build --release

BIN_FILE="target/thumbv7em-none-eabihf/release/ocd-stm32f3"

if [ ! -f "$BIN_FILE" ]; then
    echo "Error: Binary not found at $BIN_FILE"
    exit 1
fi

echo "Waiting for DFU device..."
sleep 2

echo "Flashing firmware..."
dfu-util -a 0 -s 0x08000000:leave -D "$BIN_FILE"

echo "Done!"