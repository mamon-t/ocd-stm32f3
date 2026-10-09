#!/bin/bash
set -e

echo "Building firmware..."
cargo build --release

# target — thumbv7em-none-eabi (soft-float; FPU у STM32F303 есть, но в прошивке не используется)
ELF="target/thumbv7em-none-eabi/release/ocd-stm32f3"
BIN="target/firmware.bin"

if [ ! -f "$ELF" ]; then
    echo "Error: ELF not found at $ELF"
    exit 1
fi

# .bin для dfu-util (elf не подойдёт)
arm-none-eabi-objcopy -O binary "$ELF" "$BIN"

# Как попасть в bootloader-режим (DFU):
#   аппаратно: перевести BOOT0 в 1 (паяльный мост SB19) и нажать RESET (B2).
#   По умолчанию SB19 разомкнут, BOOT0 подтянут к GND (510 Ом) — старт из
#   основной flash. Программной команды входа в DFU в прошивке НЕТ: по RM0316
#   режим загрузки на STM32F3 определяется состоянием пина BOOT0 при сбросе
#   и не переключается регистром SYSCFG_MEMRMP.
echo "Waiting for DFU device (Ctrl-C чтобы отменить)..."
until lsusb | grep -q "0483:df11"; do sleep 0.5; done
echo "DFU device found."

echo "Flashing firmware..."
dfu-util -a 0 -s 0x08000000:leave -D "$BIN"

echo "Done!"