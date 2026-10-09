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
//   - ACK ответов нет: команды применяются атомарно, подтверждением служит
//     следующая команда GET_MASK / изменение потока кадров. Один Writer
//     исключает гонку за IN-эндпоинт между задачами.
//   - Общая конфигурация каналов за critical-section mutex; задача потока
//     читает копию перед каждой сборкой кадра.
//   - DFU-команда ставит magic в BKP0R и делает sys_reset; приложение при
//     старте ОБЯЗАТЕЛЬНО сбрасывает BKP0R (иначе любой рестарт кидал бы в
//     бутлоадер снова — баг прежней прошивки).
// ---------------------------------------------------------------------------

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};
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
use embassy_time::Timer;
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
//                   0xD0 (рейд в DFU)
// Ответ на 0xB2:    0xB3 <mask_lo> <mask_hi>
const DMA_BUF_SIZE: usize = 512;
const NUM_CHANNELS: usize = 10;
// ADC1 Common Interface (STM32F3): DR на смещении 0x48, НЕ 0x40 (там SR).
const ADC1_BASE: usize = 0x5000_0000;
const ADC1_DR_ADDR: usize = ADC1_BASE + 0x48;
const ADC1_SQR1_ADDR: usize = ADC1_BASE + 0x28; // регистр длины последовательности L[4:0]
const FRAME_LEN: usize = 5 + NUM_CHANNELS * 2;

const ADC_HW_CHANNELS: [u8; NUM_CHANNELS] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

const DATA_SYNC: [u8; 2] = [0xAA, 0x55];
const CMD_SET_RATE: u8 = 0xA0;
const CMD_SET_CHANNEL: u8 = 0xB0;
const CMD_GET_MASK: u8 = 0xB2;
const CMD_ENTER_DFU: u8 = 0xD0;
const STATUS_FRAME: u8 = 0x40; // бит в flags: это статус-кадр (ответ на 0xB2), не данные

const DFU_MAGIC: u32 = 0xDEADBEEF;
// Адрес System Memory (ROM-загрузчик) F303: 0x1FFFD800. Прямой прыжок туда не
// используется — вместо него штатный рейд через SYSCFG BOOT_EN + sys_reset
// (см. enter_dfu_mode). Оставлено как справка.
#[allow(dead_code)]
const SYSTEM_BOOTLOADER_ADDR: u32 = 0x1FFFD800;
const BKP0R_ADDR: usize = 0x4000_2850;

#[derive(Clone, Copy)]
struct ChannelCfg {
    enabled: bool,
    sample_time: u8,
}

/// Флаг «хост запросил маску» (ставит cmd_task, сбрасывает data_task).
static ACK_PENDING: AtomicBool = AtomicBool::new(false);

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

/// Вызов из `main` ДО инициализации периферии: если бутлоадер запрошен —
/// прыгаем в системный memory bootloader; иначе чистим magic, чтобы следующий
/// обычный рестарт не угодил в бутлоадер повторно.
fn check_and_enter_bootloader() {
    unsafe {
        let bkp0r = BKP0R_ADDR as *mut u32;
        let magic = core::ptr::read_volatile(bkp0r);
        if magic == DFU_MAGIC {
            // Сбрасываем magic и уходим в системный memory bootloader по
            // официальной процедуре AN4536 (STM32F3): BL=1, nBOOT_SEL/nBOOT1/
            // nBOOT0=0, скидываем все активные конфиги SYSCFG, гасим USB PHY
            // (D+ pull-up остаётся поднятым -> хост видит "DFU-mode device"),
            // глобальный reset. Boot ROM на сбросе читает эти биты и стартует
            // из SystemMemory.
            core::ptr::write_volatile(bkp0r, 0);
            cortex_m::interrupt::disable();

            let syscfg = 0x4001_0000usize as *mut u32; // SYSCFG->MEMRMP
            (*syscfg) &= !(1 << 8 | 1 << 9 | 1 << 10 | 1 << 11); // BL | nBOOT_SEL | nBOOT1 | nBOOT0
            *(0x4001_0004usize as *mut u32) = 0; // SYSCFG->UR_SET: clear CFGRST

            let usb = 0x5000_0000usize as *mut u32; // USB base
            let cnf = *(usb as *const u32); // CNF = offset 0x00
            *(usb as *mut u32) = cnf & !(1 << 23); // clear FORCE_FMODS -> internal PU off
            *(usb.add(0x40 / 4)) |= 1 << 15; // PDWN in KCSR

            cortex_m::peripheral::SCB::sys_reset();
        }
        // Магическое значение протухло сразу после обычной проверки.
        core::ptr::write_volatile(bkp0r, 0);
    }
}

fn enter_dfu_mode() -> ! {
    unsafe {
        // 1) Magic в BKP0R — на случай, если загрузчик/внешний скрипт проверяет его.
        core::ptr::write_volatile(BKP0R_ADDR as *mut u32, DFU_MAGIC);
        // 2) Штатный способ STM32F3: SYSCFG->MEMRMP bit BOOT_EN (0x40010000 + 0x00),
        //    затем soft-reset. ROM-загрузчик сам сбросит этот бит при выходе в app.
        const SYSCFG_MEMRMP: usize = 0x4001_0000;
        core::ptr::write_volatile(SYSCFG_MEMRMP as *mut u32,
            core::ptr::read_volatile(SYSCFG_MEMRMP as *const u32) | 1); // BOOT_EN
        cortex_m::peripheral::SCB::sys_reset();
    }
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
    unsafe {
        // SQR1[4:0] = L (длина-1); SQ bits стартуют с бита 6.
        let mut sqr1: u32 = ((n as u32) - 1) & 0x1F;
        for i in 0..n.min(4) {
            sqr1 |= (seq_list[i] as u32) << (6 + i * 5);
        }
        core::ptr::write_volatile(ADC1_SQR1_ADDR as *mut u32, sqr1);
    }
    if n > 4 {
        unsafe {
            let mut sqr2: u32 = 0;
            for i in 0..(n - 4).min(4) {
                sqr2 |= (seq_list[4 + i] as u32) << (i * 5);
            }
            core::ptr::write_volatile((ADC1_BASE + 0x2C) as *mut u32, sqr2);
        }
    }
    if n > 8 {
        unsafe {
            let mut sqr3: u32 = 0;
            for i in 0..(n - 8) {
                sqr3 |= (seq_list[8 + i] as u32) << (i * 5);
            }
            core::ptr::write_volatile((ADC1_BASE + 0x30) as *mut u32, sqr3);
        }
    }
    pac::ADC1.sqr2().modify(|w| {
        for i in 0..4 {
            w.set_sq(i, ADC_HW_CHANNELS[4 + i]);
        }
    });
    pac::ADC1.sqr3().modify(|w| {
        for i in 0..2 {
            w.set_sq(i, ADC_HW_CHANNELS[8 + i]);
        }
    });

    // SMPR1: каналы 1-9, SMPR2: канал 0
    pac::ADC1.smpr1().modify(|w| {
        for ch in 1..=9 {
            w.set_smp(ch, st);
        }
    });
    pac::ADC1.smpr2().modify(|w| {
        w.set_smp(0, st);
    });

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
        defmt::info!("USB host connected (DTR/RTS)");
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

            if ACK_PENDING.swap(false, Ordering::Relaxed) {
                let cfgs = channel_snapshot();
                let mut mask_lo = 0u8;
                let mut mask_hi = 0u8;
                for (i, c) in cfgs.iter().enumerate() {
                    if c.enabled {
                        if i < 8 { mask_lo |= 1 << i } else { mask_hi |= 1 << (i - 8) }
                    }
                }
                let _ = tx.write_packet(&[DATA_SYNC[0], DATA_SYNC[1], STATUS_FRAME, seq, mask_lo, mask_hi]).await;
            }
            if tx.write_packet(&frame[..idx]).await.is_err() {
                break; // хост отвалился — переждём reconnect
            }
        }
    }
}

/// Приём команд: живёт отдельно от потока, блокирующее read_packet здесь
/// безопасно (поток кадров в своей задаче не стоит). Единственный ответ на
/// 0xB2 — "статус-кадр" с тем же sync-префиксом 0xAA55, но с флагом bit6
/// (STATUS_FRAME): [0xAA,0x55,0x40,seq,mask_lo,mask_hi]. Он идёт тем же
/// IN-эндпоинтом, что и данные; десктоп различает типы по флагу.
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
            }
            CMD_SET_CHANNEL if n >= 4 => {
                let ch = buf[1] as usize;
                let enable = buf[2] != 0;
                let st = buf[3].min(7);
                if ch < NUM_CHANNELS {
                    set_channel_cfg(ch, enable, st);
                    adc_stop();
                    unsafe { core::ptr::write_volatile(scan_len, adc_write_sequence(st)) };
                }
            }
            CMD_GET_MASK => {
                // Единственный писатель IN-эндпоинта — data_task. Здесь лишь
                // выставляем флаг; data_task вставит статус-кадр перед следующим
                // кадровым пакетом. Это исключает гонку за ENDPOINT_IN.
                ACK_PENDING.store(true, Ordering::Relaxed);
            }
            CMD_ENTER_DFU => {
                // Дать стеку USB дотянуть ACK до хоста перед ребутом.
                Timer::after_millis(50).await;
                enter_dfu_mode();
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
    check_and_enter_bootloader();

    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hse = Some(Hse {
            freq: mhz(8),
            mode: HseMode::Oscillator,
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
    // DMA-канал для ADC1: на STM32F3 (DMA v2 без DMAMUX) request ADC1 жёстко
    // привязан к каналу 1 контроллера DMA1.
    let adc_dma = Channel::new(p.DMA1_CH1, Irqs);

    {
        use embassy_stm32::pac;
        use embassy_stm32::pac::gpio::vals::Moder;

        pac::GPIOA.moder().modify(|w| {
            for pin in 0..4 {
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
    // Поток кадров и приём команд — независимые futures внутри join3.
    // Единственный Writer IN-эндпоинта — data_task; cmd_task общается с ней
    // через ACK_PENDING (см. комментарий в cmd_task).
    defmt::info!("firmware up: waiting for USB host");
    join3(usb_fut, data_task(tx, adc_dma, sl), cmd_task(rx, sl)).await;
}
