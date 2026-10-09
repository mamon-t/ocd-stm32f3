#![no_std]
#![no_main]

// ---------------------------------------------------------------------------
// Прошивка STM32F303VC: осциллограф-лаборатория (протокол v2)
// embassy-stm32 0.6.0 / embassy-usb 0.6.0 / embassy-executor 0.10.0
// (тег embassy-stm32-v0.6.0, rev 84444a19).
//
// Архитектура передачи данных:
//   - CDC-ACM разделён через split(): Sender живёт в задаче потока кадров,
//     Receiver — в задаче команд. Приём команд больше не тормозит поток.
//   - Все ACK (A1/B1/B3 + периодический A4) уходят строго через Sender
//     задачи потока кадров (ACK-очередь): один Writer исключает гонку
//     за IN-эндпоинт между задачами.
//   - Общая конфигурация каналов за critical-section mutex; задача потока
//     читает копию перед каждой сборкой кадра.
//   - Вход в DFU — только аппаратно: BOOT0=1 (SB19) + RESET. Команды 0xD0
//     в прошивке нет (механизм с magic в BKP0R/SYSCFG не соответствует
//     RM0316: режим загрузки определяется пином BOOT0 и не переключается
//     софтовым битом SYSCFG_MEMRMP).
//   - Тестовый генератор: программная развёртка сигнала через DAC1_OUT1 (PA4),
//     20 кГц / фиксированная 1 кГц. Управление — команда 0xC0 (см. ниже).
//   - LED-индикация (LD… на PE8..PE15, активный высокий уровень):
//     LD4 (PE8, синий) — «прошивка жива» (мигание heartbeat);
//     LD5 (PE10, оранж.) — USB-CDC подключён;
//     LD6 (PE15, зелёный) — идут кадры данных;
//     LD7 (PE11, зелёный) — тестовый генератор включён;
//     LD10 (PE13, красный) — тревога (overrun/потеря ACK), мигает ~4 с.
// ---------------------------------------------------------------------------

use core::cell::RefCell;
use core::sync::atomic::{AtomicU8, Ordering};
// Критические секции через cortex-m (в Cargo.toml включён feature
// "critical-section-single-core" — он реализует глобальный критический мьютекс).
use critical_section::{with, Mutex};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_stm32::dma::{Channel, TransferOptions};
// ReadableRingBuffer живёт в dma_bdma и реэкспортируется из embassy_stm32::dma.
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::time::mhz;
use embassy_stm32::usb::Driver;
use embassy_stm32::{bind_interrupts, peripherals, usb, Config};
use embassy_time::{Instant, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
use embassy_usb::Builder;
// Транспорт defmt: предоставляет символы _defmt_write/_defmt_acquire и пр.
use defmt_rtt as _;

// Обработчик паники даёт crate panic-probe (features=["print-defmt"]):
// 1) он определяет #[panic_handler] (иначе "panic_handler function required");
// 2) с фичей print-defmt он же экспортирует _defmt_panic, который требуют
//    embassy-* с фичей "defmt" (обычный panic-halt этот символ НЕ даёт).
use panic_probe as _;

bind_interrupts!(struct Irqs {
    USB_LP_CAN_RX0 => usb::InterruptHandler<peripherals::USB>;
    DMA1_CHANNEL1 => embassy_stm32::dma::InterruptHandler<peripherals::DMA1_CH1>;
});

// Протокол v2 (соглашение с desktop/labbench):
// Кадр MCU->PC, ровно 5 + 2*nch байт:
//   [0]=0xAA [1]=0x55 [2]=flags(bit7=ADC overrun) [3]=seq:u8 [4]=nch(1..10)
//   [5..] = отсчёты u16 LE только включённых каналов по возрастанию номера.
// Команды PC->MCU:  0xA0 <rate> | 0xB0 <ch> <en> <st> | 0xB2 (запрос маски)
//                   | 0xC0 <cfg> — тестовый генератор DAC1: бит0=вкл,
//                     биты[2:1]=форма (0=const, 1=синус, 2=треугольник,
//                     3=меандр), биты[5:3]=амплитуда 0..7 (peak=128*(n+1) кода)
// ACK MCU->PC:      0xA1 <level> | 0xB1 <ch> <en> <st> | 0xB3 <mask_lo> <mask_hi>
//                   периодически 0xA4 <fs:u32 LE> — измеренный темп сканов (Гц)
const DMA_BUF_SIZE: usize = 512;
const NUM_CHANNELS: usize = 10;
// ADC1 (RM0316 §15.5): регистр данных DATA[15:0] на смещении 0x40
// (0x00 ISR, 0x04 IER, 0x08 CR, 0x0C CFGR, 0x14 SMPR1, 0x18 SMPR2,
//  0x20/0x24/0x28 TR1/TR2/TR3, 0x30 SQR1, 0x34 SQR2, 0x38 SQR3, 0x3C SQR4,
//  0x40 DR, 0x4C JSQR).
const ADC1_BASE: usize = 0x5000_0000;
const ADC1_DR_ADDR: usize = ADC1_BASE + 0x40;
const FRAME_LEN: usize = 5 + NUM_CHANNELS * 2;

// Номера каналов ADC1 для выводов PA0, PA1, PA2, PA3, PF4, PC0, PC1, PC2, PC3, PF2
// (порядок GPIO -> номер канала согласно SVD STM32F303VC: PA0=1 ... PF2=10).
const ADC_HW_CHANNELS: [u8; NUM_CHANNELS] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

const DATA_SYNC: [u8; 2] = [0xAA, 0x55];
const CMD_SET_RATE: u8 = 0xA0;
const CMD_SET_CHANNEL: u8 = 0xB0;
const CMD_GET_MASK: u8 = 0xB2;
const CMD_SET_GEN: u8 = 0xC0; // конфигурация тестового генератора (см. шапку)
const ACK_RATE: u8 = 0xA1; // ответ на 0xA0: применённый уровень времени выборки
const ACK_CHANNEL: u8 = 0xB1; // ответ на 0xB0: <ch> <en> <st>
const ACK_MASK: u8 = 0xB3; // ответ на 0xB2: маска включённых каналов (mask_lo, mask_hi)
const ACK_FS: u8 = 0xA4; // измеренная частота сканирования, u32 LE (Гц)

// Конфигурация тестового генератора (raw-байт команды 0xC0).
static GEN_CFG: AtomicU8 = AtomicU8::new(0);
// Флаги состояния для LED-индикации (см. led_task): USB подключён / данные / тревога.
const ST_USB: u8 = 1 << 0;
const ST_DATA: u8 = 1 << 1;
const ST_ALARM: u8 = 1 << 2;
static STATUS: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy)]
struct ChannelCfg {
    enabled: bool,
    sample_time: u8,
}

/// Очередь ACK-пакетов: команды кладут ответы здесь, а единственный писатель
/// IN-эндпоинта (data_task) вычитывает и отправляет их перед очередным кадром.
const ACK_QUEUE_LEN: usize = 16;
const ACK_MAX_LEN: usize = 5;

#[derive(Clone, Copy)]
struct AckPkt {
    data: [u8; ACK_MAX_LEN],
    len: u8,
}

struct AckQueue {
    slots: [AckPkt; ACK_QUEUE_LEN],
    head: usize,
    count: usize,
}

impl AckQueue {
    const fn new() -> Self {
        Self {
            slots: [AckPkt { data: [0; ACK_MAX_LEN], len: 0 }; ACK_QUEUE_LEN],
            head: 0,
            count: 0,
        }
    }
    fn push(&mut self, pkt: &[u8]) {
        if self.count >= ACK_QUEUE_LEN {
            return;
        }
        let idx = (self.head + self.count) % ACK_QUEUE_LEN;
        let n = pkt.len().min(ACK_MAX_LEN);
        self.slots[idx].data[..n].copy_from_slice(&pkt[..n]);
        self.slots[idx].len = n as u8;
        self.count += 1;
    }
    fn pop(&mut self) -> Option<AckPkt> {
        if self.count == 0 {
            return None;
        }
        let idx = self.head;
        self.head = (self.head + 1) % ACK_QUEUE_LEN;
        self.count -= 1;
        Some(self.slots[idx])
    }
}

static ACK_QUEUE: Mutex<RefCell<AckQueue>> = Mutex::new(RefCell::new(AckQueue::new()));

fn push_ack(pkt: &[u8]) {
    with(|cs| ACK_QUEUE.borrow_ref_mut(cs).push(pkt));
}

static CH_CFG: Mutex<RefCell<[ChannelCfg; NUM_CHANNELS]>> = Mutex::new(RefCell::new([ChannelCfg {
    enabled: true,
    sample_time: 2,
}; NUM_CHANNELS]));

fn sample_time_from_u8(val: u8) -> u8 {
    match val {
        0..=7 => val,
        _ => 2,
    }
}

/// Снимок конфигурации каналов (копируется под критической секцией).
fn channel_snapshot() -> [ChannelCfg; NUM_CHANNELS] {
    with(|cs| *CH_CFG.borrow_ref(cs))
}

fn set_channel_cfg(ch: usize, enable: bool, st: u8) {
    with(|cs| {
        let mut cfgs = CH_CFG.borrow_ref_mut(cs);
        if ch < NUM_CHANNELS {
            cfgs[ch].enabled = enable;
            cfgs[ch].sample_time = st;
        }
    });
}

fn adc_write_sequence(sample_time: u8) -> usize {
    use embassy_stm32::pac;
    use embassy_stm32::pac::adc::vals::{Advregen, Align, Dmacfg, Res, SampleTime};

    let st: SampleTime = unsafe { core::mem::transmute(sample_time_from_u8(sample_time)) };

    pac::RCC.ahbenr().modify(|w| w.set_adc12en(true));
    pac::ADC1.cr().modify(|w| w.set_advregen(Advregen::INTERMEDIATE));
    cortex_m::asm::delay(720);
    pac::ADC1.cr().modify(|w| w.set_advregen(Advregen::ENABLED));
    cortex_m::asm::delay(720);
    pac::ADC1.cr().modify(|w| w.set_adcaldif(false));
    pac::ADC1.cr().modify(|w| w.set_adcal(true));
    while pac::ADC1.cr().read().adcal() {}
    cortex_m::asm::delay(100);
    pac::ADC1.isr().write(|w| w.set_adrdy(true));
    pac::ADC1.cr().modify(|w| w.set_aden(true));
    while !pac::ADC1.isr().read().adrdy() {}

    pac::ADC1.cfgr().modify(|w| {
        w.set_res(Res::BITS12);
        w.set_align(Align::RIGHT);
        w.set_cont(true);
        w.set_dmaen(true);
        w.set_dmacfg(Dmacfg::CIRCULAR);
        w.set_ovrmod(true);
    });

    // Динамическая длина последовательности: только включённые каналы — иначе
    // выключенный канал всё равно конвертируется и сдвигает кадры.
    let cfgs = channel_snapshot();
    let mut seq_list = [0u8; NUM_CHANNELS];
    let mut n = 0usize;
    for (i, c) in cfgs.iter().enumerate() {
        if c.enabled {
            seq_list[n] = ADC_HW_CHANNELS[i];
            n += 1;
        }
    }
    if n == 0 {
        seq_list[0] = ADC_HW_CHANNELS[0];
        n = 1;
    }
    // SQR1: L[3:0]=длина-1, SQ1..SQ4 с шагом 6 бит от бита 6 (RM0316 §15.5.10);
    // SQR2: SQ5..SQ9; SQR3: SQ10+ — значения каналов пишутся как есть.
    pac::ADC1.sqr1().modify(|w| {
        w.set_l((n as u8).wrapping_sub(1));
        for i in 0..n.min(4) {
            w.set_sq(i, seq_list[i]);
        }
    });
    if n > 4 {
        pac::ADC1.sqr2().modify(|w| {
            for i in 0..(n - 4).min(5) {
                w.set_sq(i, seq_list[4 + i]);
            }
        });
    }
    if n > 9 {
        pac::ADC1.sqr3().modify(|w| {
            w.set_sq(0, seq_list[9]);
        });
    }
    // SMPR1: каналы 1..9 -> поле SMP1..SMP9 (индекс = номер канала - 1);
    // SMPR2: индекс 0 -> канал 10 (RM0316 §15.5.8-9).
    for c in 1..=9u8 {
        pac::ADC1.smpr1().modify(|w| w.set_smp((c - 1) as usize, st));
    }
    pac::ADC1.smpr2().modify(|w| w.set_smp(0, st));

    pac::ADC1.cr().modify(|w| w.set_adstart(true));
    n
}

fn adc_stop() {
    use embassy_stm32::pac;
    pac::ADC1.cr().modify(|w| w.set_adstp(true));
    while pac::ADC1.cr().read().adstp() {}
}

// ---------------------------------------------------------------------------
// Задачи
// ---------------------------------------------------------------------------

/// Сердцебиение: индикатор того, что исполнитель жив и задачи переключаются.
/// Отдельная задача нужна потому, что data_task может вечно висеть на
/// wait_connection(), а cmd_task — на read_packet(); без heartbeat непонятно,
/// завис чип вообще или просто нет хоста.
#[embassy_executor::task]
async fn heartbeat_task(mut led: Output<'static>) -> ! {
    loop {
        led.toggle();
        Timer::after_millis(500).await;
    }
}

/// Светодиодная индикация состояния (ведутся атомарным флагом STATUS):
/// LD5 (PE10) — USB-CDC подключён; LD6 (PE15) — идут кадры данных;
/// LD10 (PE13) — тревога (overrun/потеря ACK), мигает и сама гаснет через ~4 с.
#[embassy_executor::task]
async fn led_task(
    mut ld_usb: Output<'static>,
    mut ld_data: Output<'static>,
    mut ld_alarm: Output<'static>,
) -> ! {
    let mut tick = 0u32;
    loop {
        let st = STATUS.load(Ordering::Relaxed);
        if st & ST_USB != 0 {
            ld_usb.set_high();
        } else {
            ld_usb.set_low();
        }
        if st & ST_DATA != 0 {
            ld_data.set_high();
        } else {
            ld_data.set_low();
        }
        if st & ST_ALARM != 0 {
            if tick % 4 == 0 {
                ld_alarm.set_high();
            } else {
                ld_alarm.set_low();
            }
            if tick >= 40 {
                STATUS.fetch_and(!ST_ALARM, Ordering::Relaxed);
            }
        } else {
            ld_alarm.set_low();
        }
        tick = (tick + 1) % 80;
        Timer::after_millis(100).await;
    }
}

/// Тестовый генератор сигналов: программная развёртка через DAC1_OUT1 (PA4).
/// Такт 20 кГц (Timer::after_micros(50)), длительность периода 20 отсчётов =>
/// фиксированная частота 1 кГц. Форма (биты [2:1] GEN_CFG) и амплитуда
/// ([5:3], peak = 128*(n+1) кода ЛШИ) задаются командой 0xC0. LD7 (PE11)
/// горит, пока генератор включён.
#[embassy_executor::task]
async fn generator_task(mut led_gen: Output<'static>) -> ! {
    use embassy_stm32::pac;

    const PHASES: usize = 20;
    const HALF: usize = PHASES / 2;
    // sin(2π·i/20)·2048, i=0..19 (одна таблица на период — фиксированная 1 кГц).
    const SINE: [i32; PHASES] = [
        0, 633, 1204, 1657, 1948, 2048, 1948, 1657, 1204, 633, 0, -633, -1204, -1657, -1948, -2048,
        -1948, -1657, -1204, -633,
    ];

    let mut ph = 0usize;
    loop {
        let cfg = GEN_CFG.load(Ordering::Relaxed);
        if cfg & 1 == 0 {
            led_gen.set_low();
            Timer::after_millis(50).await;
            continue;
        }
        led_gen.set_high();
        let shape = (cfg >> 1) & 0x3;
        let amp = 128 * (((cfg >> 3) & 0x7) as i32 + 1);
        let s = match shape {
            1 => SINE[ph],
            2 => {
                let m = ph % HALF;
                let tri = if m <= HALF / 2 {
                    (m as i32 * 4096) / (HALF as i32 / 2)
                } else {
                    ((HALF as i32 - m as i32) * 4096) / (HALF as i32 / 2)
                };
                tri - 2048
            }
            3 => {
                if ph < HALF {
                    -2048
                } else {
                    2048
                }
            }
            _ => 0,
        };
        let code = (2048 + s * amp / 2048).clamp(0, 4095);
        pac::DAC1.dhr12r(0).modify(|w| w.set_dhr(code as u16));
        ph = (ph + 1) % PHASES;
        Timer::after_micros(50).await;
    }
}

/// Поток кадров: ждём полный скан в DMA-кольце, собираем кадр, отправляем.
async fn data_task<'d>(
    mut tx: Sender<'d, Driver<'d, peripherals::USB>>,
    dma_ch: Channel<'d>,
    scan_len: *mut usize,
) -> ! {
    static mut DMA_BUF: [u16; DMA_BUF_SIZE] = [0u16; DMA_BUF_SIZE];

    let mut ring_buf = unsafe {
        let ring = &mut *(&raw mut DMA_BUF);
        // На F3 (dma_v2 без DMAMUX) линия запроса выбирается аппаратно по
        // номеру канала: ADC1 = DMA1 CH1. Аргумент request на этом семействе —
        // unit-тип Request = () (см. embassy-stm32/src/dma/mod.rs).
        embassy_stm32::dma::ReadableRingBuffer::new(
            dma_ch,
            (),
            ADC1_DR_ADDR as *mut u16,
            ring,
            TransferOptions::default(),
        )
    };
    ring_buf.set_alignment(unsafe { core::ptr::read_volatile(scan_len).max(1) });
    ring_buf.start();

    let mut seq: u8 = 0;
    let mut scan_buf = [0u16; NUM_CHANNELS];
    let mut frame = [0u8; FRAME_LEN];

    loop {
        tx.wait_connection().await;
        STATUS.fetch_or(ST_USB, Ordering::Relaxed);
        defmt::info!("USB host connected (DTR/RTS)");
        let mut fs_frames: u64 = 0;
        let mut fs_start = Instant::now();
        loop {
            let want = unsafe { core::ptr::read_volatile(scan_len).max(1) };
            match ring_buf.len() {
                Ok(available) if available >= want => {}
                _ => {
                    Timer::after_micros(100).await;
                    continue;
                }
            }
            let got = ring_buf.read_latest(&mut scan_buf[..want]);
            if got < want {
                continue;
            }

            let ovr = embassy_stm32::pac::ADC1.isr().read().ovr();
            if ovr {
                STATUS.fetch_or(ST_ALARM, Ordering::Relaxed);
            }

            let mut idx = 5usize;
            let mut nch = 0u8;
            for v in &scan_buf[..want] {
                frame[idx] = *v as u8;
                frame[idx + 1] = (*v >> 8) as u8;
                idx += 2;
                nch += 1;
            }
            frame[0] = DATA_SYNC[0];
            frame[1] = DATA_SYNC[1];
            frame[2] = if ovr { 0x80 } else { 0 };
            seq = seq.wrapping_add(1);
            frame[3] = seq;
            frame[4] = nch;

            let mut ack_failed = false;
            while let Some(pkt) = with(|cs| ACK_QUEUE.borrow_ref_mut(cs).pop()) {
                if tx.write_packet(&pkt.data[..pkt.len as usize]).await.is_err() {
                    ack_failed = true;
                    break;
                }
            }
            if ack_failed {
                STATUS.fetch_and(!(ST_USB | ST_DATA), Ordering::Relaxed);
                break;
            }

            if tx.write_packet(&frame[..idx]).await.is_err() {
                STATUS.fetch_and(!(ST_USB | ST_DATA), Ordering::Relaxed);
                break; // хост отвалился — переждём reconnect
            }
            STATUS.fetch_or(ST_DATA, Ordering::Relaxed);

            fs_frames += 1;
            let elapsed = fs_start.elapsed();
            if elapsed.as_millis() >= 500 {
                let us = elapsed.as_micros().max(1);
                let fs = ((fs_frames * 1_000_000) / us) as u32;
                fs_frames = 0;
                fs_start = Instant::now();
                let pkt = [ACK_FS, fs as u8, (fs >> 8) as u8, (fs >> 16) as u8, (fs >> 24) as u8];
                if tx.write_packet(&pkt).await.is_err() {
                    STATUS.fetch_and(!(ST_USB | ST_DATA), Ordering::Relaxed);
                    break;
                }
            }
        }
    }
}

/// Приём команд: живёт отдельно от потока, блокирующее read_packet здесь
/// безопасно (поток кадров в своей задаче не стоит). Ответы (A1/B1/B3/D1)
/// кладутся в ACK_QUEUE, а отправляет их единственный писатель IN-эндпоинта —
/// data_task.
async fn cmd_task<'d>(mut rx: Receiver<'d, Driver<'d, peripherals::USB>>, scan_len: *mut usize) -> ! {
    let mut buf = [0u8; 64];
    loop {
        let n = match rx.read_packet(&mut buf).await {
            Ok(n) => n,
            Err(_) => {
                Timer::after_millis(50).await;
                continue;
            }
        };
        if n == 0 {
            continue;
        }
        match buf[0] {
            CMD_SET_RATE if n >= 2 => {
                let level = buf[1].min(7);
                adc_stop();
                unsafe { core::ptr::write_volatile(scan_len, adc_write_sequence(level)) };
                push_ack(&[ACK_RATE, level]);
            }
            CMD_SET_CHANNEL if n >= 4 => {
                let ch = buf[1] as usize;
                let enable = buf[2] != 0;
                let st = buf[3].min(7);
                if ch < NUM_CHANNELS {
                    set_channel_cfg(ch, enable, st);
                    adc_stop();
                    unsafe { core::ptr::write_volatile(scan_len, adc_write_sequence(st)) };
                    push_ack(&[ACK_CHANNEL, buf[1], buf[2], buf[3]]);
                }
            }
            CMD_GET_MASK => {
                let cfgs = channel_snapshot();
                let mut mask_lo = 0u8;
                let mut mask_hi = 0u8;
                for (i, c) in cfgs.iter().enumerate() {
                    if c.enabled {
                        if i < 8 { mask_lo |= 1 << i } else { mask_hi |= 1 << (i - 8) }
                    }
                }
                push_ack(&[ACK_MASK, mask_lo, mask_hi]);
            }
            CMD_SET_GEN if n >= 2 => {
                GEN_CFG.store(buf[1], Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

// usb_force_wake() удалён: в используемой ревизии embassy-stm32 (84444a1)
// сам Driver::new делает `w.set_pdwn(false)` и полный сброс модуля
// (embassy-stm32/src/usb/usb.rs:319), а pull-up/D+ включает CdcAcmClass::start().
// Ручное копание в регистрах только конфликтовало с HAL к тому же оно не
// компилировалось (в pac этой ревизии поля называются иначе).

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        // HSE 8 МГц подаётся в режиме Bypass с выхода MCO ST-LINK на PF0-OSC_IN
        // (на плате нет кварца X2; конфигурация по умолчанию макета, см. UM1570
        // §4.10.1 и Project/Demonstration/system_stm32f30x.c demo-прошивки ST).
        config.rcc.hse = Some(Hse {
            freq: mhz(8),
            mode: HseMode::Bypass,
        });
        config.rcc.pll = Some(Pll {
            src: PllSource::HSE,
            prediv: PllPreDiv::DIV1,
            mul: PllMul::MUL9,
        });
        config.rcc.sys = Sysclk::PLL1_P;
        config.rcc.ahb_pre = AHBPrescaler::DIV1;
        config.rcc.apb1_pre = APBPrescaler::DIV2;
        config.rcc.apb2_pre = APBPrescaler::DIV1;
        // PLL 72 МГц / 1.5 = 48 МГц — обязательная частота для USB.
        // В embassy-stm32 0.6.0 для F3-семейства делитель применяется
        // автоматически при sysclk==72 МГц (см. rcc/f013.rs).
    }

    let p = embassy_stm32::init(config);


    let led = Output::new(p.PE8, Level::Low, Speed::Low);
    let ld_usb = Output::new(p.PE10, Level::Low, Speed::Low);
    let ld_data = Output::new(p.PE15, Level::Low, Speed::Low);
    let ld_alarm = Output::new(p.PE13, Level::Low, Speed::Low);
    let ld_gen = Output::new(p.PE11, Level::Low, Speed::Low);
    // DMA-канал для ADC1: на STM32F3 (DMA v2 без DMAMUX) request ADC1 жёстко
    // привязан к каналу 1 контроллера DMA1.
    let adc_dma = Channel::new(p.DMA1_CH1, Irqs);

    {
        use embassy_stm32::pac;
        use embassy_stm32::pac::gpio::vals::Moder;

        pac::GPIOA.moder().modify(|w| {
            for pin in 0..=4 {
                w.set_moder(pin, Moder::ANALOG);
            }
        });
        pac::GPIOC.moder().modify(|w| {
            for pin in 0..4 {
                w.set_moder(pin, Moder::ANALOG);
            }
        });
        pac::GPIOF.moder().modify(|w| {
            w.set_moder(2, Moder::ANALOG);
            w.set_moder(4, Moder::ANALOG);
        });
    }

    let scan_len = adc_write_sequence(2);

    // DAC1 (тестовый генератор, PA4): такт APB1 + выходной буфер канала 1.
    // Сдвиг выходного кода и форма — за генератором в generator_task.
    {
        use embassy_stm32::pac;
        pac::RCC.apb1enr().modify(|w| w.set_dacen(true));
        pac::DAC1.cr().modify(|w| {
            w.set_boff(0, false);
            w.set_en(0, true);
        });
    }

    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);
    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("Embassy");
    usb_config.product = Some("STM32F3 LabBench");
    usb_config.serial_number = Some("001");

    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    // CDC требует control_buf >= 7 (assert в CdcAcmClass::new).
    let mut control_buf = [0; 7];
    let mut state = State::new();

    let mut builder = Builder::new(
        driver,
        usb_config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [],
        &mut control_buf,
    );

    let class = CdcAcmClass::new(&mut builder, &mut state, 64);
    let (tx, rx) = class.split();
    let mut usb = builder.build();

    let usb_fut = usb.run();

    static mut SCAN_LEN: usize = 0;
    unsafe { core::ptr::write_volatile(&raw mut SCAN_LEN, scan_len) };
    let sl = &raw mut SCAN_LEN as *mut usize;
    // Heartbeat-задача: если светодиод НЕ моргает — исполнитель/чип зависли
    // (например, hard fault до входа в main). Если моргает, но данных нет —
    // проблема в USB-подключении, это видно по логу ниже.
    spawner.spawn(heartbeat_task(led).unwrap());
    spawner.spawn(led_task(ld_usb, ld_data, ld_alarm).unwrap());
    spawner.spawn(generator_task(ld_gen).unwrap());
    // Поток кадров и приём команд — независимые futures внутри join3.
    // Единственный Writer IN-эндпоинта — data_task; cmd_task общается с ней
    // через ACK_QUEUE (см. комментарий в cmd_task).
    defmt::info!("firmware up: waiting for USB host");
    join3(usb_fut, data_task(tx, adc_dma, sl), cmd_task(rx, sl)).await;
}
