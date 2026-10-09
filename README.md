# LabBench — STM32F3Discovery USB measurement bench

A USB laboratory instrument built on the **STM32F3DISCOVERY** board
(**STM32F303VCT6**, 72 MHz Cortex-M4F): a 10-channel oscilloscope with a spectrum
analyzer, an LCR / impedance meter and a transient (rise-time) recorder.

Firmware is written in **async Rust on Embassy** (embassy-stm32 0.6.0), the
desktop frontend **labbench** is a native GUI written in **Rust / gpui-kit
0.7.1** (Zed UI framework). No Python, no GTK.

## Layout

```
.
├── firmware/                # Embassy firmware for STM32F303VC (protocol v2)
│   ├── src/main.rs          # ADC1 + DMA1_CH1 + USB CDC-ACM, command/ACK loop
│   ├── src/test_usb.rs      # Standalone USB smoke test (NOT part of the build)
│   ├── memory.x             # FLASH 256K @ 0x0800_0000, RAM 40K @ 0x2000_0000
│   ├── .cargo/config.toml   # target thumbv7em-none-eabi + link.x/defmt.x
│   └── flash.sh             # Build + flash via dfu-util (DFU bootloader)
├── desktop/                 # "labbench" GUI (gpui-kit 0.7.1)
│   ├── src/main.rs          # Tabs: Oscilloscope · Spectrum · LCR · Transient
│   ├── src/canvas.rs        # TraceCanvas: grid / trigger / per-channel traces
│   ├── src/dsp.rs           # FFT (dBFS), trigger, LCR impedance, rise time
│   └── src/transport.rs     # Protocol v2 over USB-CDC + port auto-detect
├── 99-stm32.rules           # udev rules: ST-LINK (0483:5710/3748) + STM32 DFU
├── docs/hardware_verification_plan.md  # Manual hardware test procedure
└── tmp/                     # Local copies of official ST docs & demo firmware
```

## Board wiring (verified against UM1570 / DS9866 / RM0316)

Two USB connectors are used (both mini-B, both needed at once):

| Connector | Purpose |
|-----------|---------|
| **CN1** | USB **ST-LINK/V2** (`0483:3748`) — programming/debugging |
| **CN2** | USB **USER** — the device USB: **PA12 = USBDP, PA11 = USBDM** (CDC-ACM) |

### Clock

- **HSE: 8 MHz bypass** from the on-board ST-LINK's MCO output into
  **PF0/OSC_IN** (elastic bridge SB12; the 8 MHz crystal X2 is **not** fitted on
  the stock board, so crystal mode would never lock). MCO path per UM1570,
  confirmed by the official demo firmware (`system_stm32f30x.c` sets
  `HSEBYP`).
- PLL: 8 MHz × 9 = **72 MHz SYSCLK**, AHB ÷1, APB2 ÷1 (72 MHz), APB1 ÷2
  (36 MHz).
- USB clock: **48 MHz = 72 / 1.5** (applied automatically by embassy's RCC
  when SYSCLK = 72 MHz).

### ADC input map (ADC1, 12-bit)

| ADC channel | Pin | On P1 header |
|------------:|-----|-------------|
| 1 | PA0 | P1-12 |
| 2 | PA1 | P1-5  |
| 3 | PA2 | P1-14 |
| 4 | PA3 | P1-7  |
| 5 | PF4 | P1-9  |
| 6 | PC0 | P1-6  |
| 7 | PC1 | P1-1  |
| 8 | PC2 | P1-8  |
| 9 | PC3 | P1-3  |
| 10| PF2 | P1-10 |

Notes:

- **PA0 also carries the USER button B1** (SB20 closed by default): the pin is
  pulled up (~3.3 V) and goes low while the button is held.
- All inputs are sampled through ADC1 configured continuous + circular,
  transferred to RAM by **DMA1_CH1** (512-sample ring buffer), overrun is
  reported in-band.
- User LEDs (all driven high, LED to GND via ~510–680 Ω):
  - **LD4 (blue) = PE8** — heartbeat (blinks while the firmware is alive);
  - **LD5 (orange) = PE10** — USB CDC connected (host opened the port);
  - **LD6 (green) = PE15** — data frames are flowing;
  - **LD7 (green) = PE11** — test generator is ON;
  - **LD10 (red) = PE13** — alarm: ADC overrun/ACK loss (blinks ~4 s).

### Entering the DFU bootloader

Software DFU entry is **intentionally not implemented**. On STM32F3 the boot
mode is selected by the **BOOT0 pin** at reset (RM0316 §3.5) and cannot be
changed by writing `SYSCFG_MEMRMP`. To flash the firmware:

1. Close **SB19** (BOOT0 → 1, the pin is pulled down by 510 Ω by default).
2. Press **RESET (B2)** — the ROM bootloader appears as `0483:df11`.
3. Run `./firmware/flash.sh`.
4. Open **SB19**, press RESET again — the application starts.

## Building

Prerequisites:

- Rust (stable is fine) with target `thumbv7em-none-eabi`
  (`rustup target add thumbv7em-none-eabi`).
- For desktop builds on Linux: a desktop session (gpui needs a display/GPU)
  and `libudev` to build the `serialport` crate.
- To flash: `arm-none-eabi-objcopy` + `dfu-util`; the udev rules provide
  access to `0483:df11` for the `plugdev` group.

The embassy crates are pinned to the git revision `84444a19`
(embassy-stm32-v0.6.0 tag) — see `firmware/Cargo.toml`; do not mix versions
across the embassy cluster.

### Firmware

```bash
cd firmware
cargo build --release
./flash.sh          # builds, waits for 0483:df11, flashes via dfu-util
```

### Desktop

```bash
cd desktop
cargo build --release
cargo test          # 11 unit tests (protocol parser + DSP)
```

Run without hardware (simulator):

```bash
./target/release/labbench --simulate
```

Run against the board (CDC-ACM `ttyACM*`/`ttyUSB*`/`COMx` are auto-detected):

```bash
./target/release/labbench --port /dev/ttyACM0 --baud 115200
```

(`--baud` is accepted for compatibility; CDC-ACM ignores it.)

## Protocol over USB (v2)

### Data frame MCU → PC — `5 + 2·nch` bytes

```
AA 55 <flags> <seq> <nch> <ch0_lo> <ch0_hi> ... <ch(n-1)_lo> <ch(n-1)_hi>
```

- `AA 55` — sync marker
- `flags` — bit 7 set = ADC overrun (data skipped); other bits reserved
- `seq` — frame sequence number, wraps modulo 256 (desktop counts lost frames)
- `nch` — number of channels in this frame (1..10)
- samples — 12-bit ADC values, little-endian `u16`. The desktop maps a raw
  value `r` to units of full scale: `r / 2047.5 − 1.0`
  (0x0000 → −1.0, 0x0FFF → ~+1.0).

### Commands PC → MCU

| Command      | Opcode | Payload                                  |
|--------------|:------:|------------------------------------------|
| SET_RATE     | `0xA0` | `<level 0..7>` — 3-bit ADC sampling time |
| SET_CHANNEL  | `0xB0` | `<ch> <enable> <sample_time>`            |
| GET_MASK     | `0xB2` | —                                        |
| SET_GEN      | `0xC0` | `<cfg>` — test generator, see below      |

### ACKs MCU → PC

| ACK          | Opcode | Payload                                  |
|--------------|:------:|------------------------------------------|
| ACK_RATE     | `0xA1` | `<level>`                                |
| ACK_CHANNEL  | `0xB1` | `<ch> <enable> <sample_time>`            |
| ACK_MASK     | `0xB3` | `<mask_lo> <mask_hi>` (channel enable mask) |
| ACK_FS       | `0xA4` | `<fs: u32 LE>` — **measured** scan rate, Hz |

`ACK_FS` is emitted periodically (~0.5 s) while streaming: the firmware
measures how many scan frames it actually produced and reports the real
per-channel sample rate. The desktop uses this value directly for the FFT and
LCR math instead of assuming it from the sampling-time table — so the spectrum
and impedance readouts stay correct for every combination of enabled channels
and sample time.

All ACKs travel through the frame task's writer (single owner of the IN
endpoint), so data and ACK bytes never interleave.

## Test generator (DAC1) and LED signalling

The firmware carries a small calibration generator on **DAC1_OUT1 = PA4**
(P1-16). It is configured with the `0xC0` command — one configuration byte:

| Bit(s) | Meaning                                                        |
|--------|----------------------------------------------------------------|
| 0      | enable: 0 = off, 1 = on                                         |
| 2:1    | shape: 0 = DC mid-scale, 1 = sine, 2 = triangle, 3 = square     |
| 5:3    | amplitude level 1..8 (peak ≈ `128·n` LSB around mid-scale)      |

The signal is synthesised in software: **20 samples per period at a fixed
20 kHz tick (≈1 kHz)**, 12-bit DAC, rail-to-rail on PA4. The desktop `labbench`
exposes this as the *Generator* bar (shape buttons `Выкл/Синус/Треугольник/Меандр`
and an amplitude cycle `1..8`), so a scope probe on PA4 gives a self-test signal
without external hardware.

LED scheme is described in the board section above (heartbeat, USB, data,
generator, alarm).

## Status

- Firmware: builds with **0 warnings**; ADC1 + DMA1_CH1 verified against the
  reference manual (register map, SQR/SMPR layout, DR offset 0x40).
- Desktop: builds (a few intentionally-unused protocol/DSP helpers remain as
  dead-code warnings — they are part of the protocol API), unit tests pass.
- `firmware/src/test_usb.rs` is a legacy standalone USB test; to build it,
  uncomment the `[[bin]]` entry in `firmware/Cargo.toml`.

See `docs/hardware_verification_plan.md` (RU) for the manual end-to-end
checklist on real hardware.