# STM32F3 Oscilloscope

USB-осциллограф на STM32F3Discovery (STM32F303VCT6) + Python GTK3 фронтенд.

## Стек

| Компонент | Технология |
|-----------|-----------|
| МК | STM32F303VCT6 (Cortex-M4F, 72 MHz, 256K Flash, 40K SRAM) |
| Framework | Embassy 0.6.0 (async Rust) |
| Прошивка | ADC1 + DMA1_CH1 + USB CDC-ACM |
| Фронтенд | Python 3 + GTK3 + matplotlib |

## Структура проекта

```
.
├── Cargo.toml              # Зависимости (embassy-stm32, embassy-usb, defmt, cortex-m)
├── .cargo/config.toml      # target thumbv7em-none-eabihf, runner probe-rs
├── memory.x                # FLASH 0x08000000 256K, RAM 0x20000000 40K
├── build.rs                # Копирует memory.x в OUT_DIR для линковщика
├── flash.sh                # Один скрипт: udev rules + build + flash
├── 99-stm32.rules          # udev правила для STM32 STLink
├── src/
│   ├── main.rs             # Основная прошивка: ADC + DMA + USB CDC-ACM
│   └── test_usb.rs         # Минимальный тест USB (без ADC) — для отладки
├── gui/
│   └── oscilloscope.py     # Python GTK3 фронтенд
└── target/                 # Build artifacts
```

## Аппаратная часть

### STM32F3Discovery разъёмы

| Разъём | Тип | Назначение |
|--------|-----|-----------|
| **CN1** (сверху) | mini-USB | ST-LINK/V2 — программирование/отладка |
| **CN5** (снизу) | micro-USB | Пользовательский USB (PA11/PA12) — CDC-ACM |

**Нужны оба кабеля одновременно** для разработки.

### Распиновка ADC каналов

| Канал | Пин | GPIO |
|-------|-----|------|
| 0 | PA0 | GPIOA |
| 1 | PA1 | GPIOA |
| 2 | PA2 | GPIOA |
| 3 | PA3 | GPIOA |
| 4 | PF4 | GPIOF |
| 5 | PC0 | GPIOC |
| 6 | PC1 | GPIOC |
| 7 | PC2 | GPIOC |
| 8 | PC3 | GPIOC |
| 9 | PF2 | GPIOF |

### Тактирование

- HSE: 8 MHz bypass (от ST-LINK MCO на PA8)
- PLL: 8 MHz × 9 = 72 MHz SYSCLK
- USB: PLL / 1.5 = 48 MHz (USBPRE = DIV1_5)
- APB1: 36 MHz (DIV2)
- APB2: 72 MHz (DIV1)

## Сборка и прошивка

### Установка зависимостей

```bash
# Rust target + инструменты
rustup target add thumbv7em-none-eabihf
cargo install flip-link
cargo install probe-rs --features cli
cargo install probe-rs-tools

# Python пакеты
pip3 install pyserial matplotlib numpy
```

### Прошивка

```bash
# Вариант 1: один скрипт
./flash.sh

# Вариант 2: вручную
cargo flash --chip STM32F303VC --release
```

### Запуск фронтенда

```bash
python3 gui/oscilloscope.py
```

## Протокол обмена

### Формат пакета данных (MCU → PC)

```
[0xAA][0x55][count][sample_lo][sample_hi]...[sample_lo][sample_hi]
  sync          sync    count × 2 байта (LE uint16)
```

- `0xAA 0x55` — маркер синхронизации
- `count` — количество активных каналов в пакете (до 10)
- Каждый сэмпл: 2 байта, little-endian uint12 (0..4095)

### Команды (PC → MCU)

| Команда | Код | Параметры | Описание |
|---------|-----|-----------|----------|
| SET_RATE | 0xA0 | [level: 0-7] | Установить время выборки |
| SET_CHANNEL | 0xB0 | [ch, enable, sample_time] | Вкл/выкл канал + время выборки |
| GET_MASK | 0xB2 | — | Запрос текущей маски каналов |

### Ответы (MCU → PC)

| Ответ | Код | Данные |
|-------|-----|--------|
| ACK_RATE | 0xA1 | [level] |
| ACK_CHANNEL | 0xB1 | [ch, enable, sample_time] |
| ACK_MASK | 0xB3 | [mask_lo, mask_hi] |

### Уровни времени выборки

| Уровень | Время выборки | Частота (10 каналов) |
|---------|--------------|---------------------|
| 0 | 1.5 ADC цикла | ~1.1 Msps |
| 1 | 2.5 ADC цикла | ~800 ksps |
| 2 | 4.5 ADC цикла | ~480 ksps |
| 3 | 7.5 ADC цикла | ~290 ksps |
| 4 | 19.5 ADC цикла | ~120 ksps |
| 5 | 61.5 ADC цикла | ~40 ksps |
| 6 | 181.5 ADC цикла | ~13 ksps |
| 7 | 601.5 ADC цикла | ~4 ksps |

## Текущий статус / Известные проблемы

### Сборка
- [x] Прошивка компилируется (0 warnings)
- [x] Entry point: 0x08000189 (корректно)
- [x] .text в FLASH, .data/.bss в RAM
- [x] Линковка работает с `link.x` + `defmt.x` в `.cargo/config.toml`
- [x] udev правила установлены (99-stm32.rules)
- [x] probe-rs находит ST-LINK

### USB CDC-ACM
- [x] Код USB CDC-ACM написан (embassy-usb 0.6.0)
- [x] USB 48 MHz тактирование настроено (PLL 72 MHz / 1.5)
- [ ] **НЕ ПРОТЕСТИРОВАНО** — нет micro-USB кабеля для CN5
- [ ] При первом тесте: проверить появление `/dev/ttyACM0` в dmesg
- [ ] Если не работает: попробовать `src/test_usb.rs` (минимальный тест без ADC)

### ADC + DMA
- [x] ADC1 через PAC registers (embassy-stm32 0.6.0 не имеет ADC API для F303)
- [x] DMA1_CH1 ReadableRingBuffer для непрерывного чтения
- [x] Настройка GPIO в аналоговый режим (PA0-3, PC0-3, PF2, PF4)
- [ ] **НЕ ПРОТЕСТИРОВАНО** — зависит от USB

### Фронтенд (Python)
- [x] GTK3 + matplotlib (GTK3Agg backend)
- [x] Автопоиск serial порта
- [x] Парсинг бинарного протокола
- [x] Управление каналами (enable/disable, sample time)
- [x] Настройка частоты дискретизации
- [x] Статика (pps, напряжение по каналам)
- [ ] Тестирование с реальным устройством

## Что делать когда появится кабель

1. Подключить micro-USB кабель к **CN5** (нижний разъём)
2. Проверить: `dmesg | tail -5` — должно появитьсяttyACM0 или ошибка
3. Если `/dev/ttyACM0` появился:
   ```bash
   python3 gui/oscilloscope.py
   ```
4. Если не появился — попробовать минимальный USB тест:
   ```bash
   cp src/test_usb.rs src/main.rs
   cargo flash --chip STM32F303VC --release
   dmesg | tail -5
   ```
5. Если и тест не работает — проблема в hardware/USB, не в коде

## Ключевые решения при разработке

- **ADC1 через raw PAC registers**: `embassy-stm32 0.6.0` `adc_f3v3` variant не имеет ADC методов (всё под `#[cfg(not(adc_f3v3))]`)
- **ReadableRingBuffer для непрерывного ADC**: DMA1_CH1 читает ADC1_DR в ring buffer; `RequestType = ()` для F303
- **GPIO analog через PAC**: `set_as_analog()` не работает с `Peri` типом для F303
- **RCC ADC12 clock на AHB шине**: `pac::RCC.ahbenr().set_adc12en(true)`, не APB2ENR
- **`memory-x` feature + `link.x` + `defmt.x`**: линковщик не находит `memory.x` без явного `-Tlink.x` в `.cargo/config.toml`; `defmt.x` нужен для предоставления `_defmt_panic`
- **Нет `class.connected()`**: код рефакторится через break inner loop при ошибке `write_packet()`
- **Python GTK3**: tkinter недоступен, использован `gi.repository` Gtk3 + `matplotlib.backends.backend_gtk3agg`
