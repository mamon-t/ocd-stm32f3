#!/bin/bash
set -e

echo "Building firmware..."
cargo build --release

# target — thumbv7em-none-eabi (без eabihf: у STM32F303 нет FPU)
ELF="target/thumbv7em-none-eabi/release/ocd-stm32f3"
BIN="target/firmware.bin"

if [ ! -f "$ELF" ]; then
    echo "Error: ELF not found at $ELF"
    exit 1
fi

# .bin для dfu-util (elf не подойдёт)
arm-none-eabi-objcopy -O binary "$ELF" "$BIN"

# Как попасть в bootloader-режим (DFU):
#   вариант 1: аппаратно — замкнуть BOOT0 (перемычка JP1) на VDD и нажать RESET;
#   вариант 2: программно — прошивка сама сбрасывается в DFU по команде из UI
#              (вкладка System -> Enter DFU), после этого устройство само
#              перестроится как "STM32 BOOTLOADER".
echo "Waiting for DFU device (Ctrl-C чтобы отменить)..."
until lsusb | grep -q "0483:df11"; do sleep 0.5; done
echo "DFU device found."

echo "Flashing firmware..."
dfu-util -a 0 -s 0x08000000:leave -D "$BIN"

echo "Done!"