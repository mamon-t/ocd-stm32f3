#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_stm32::dma::{Channel, ReadableRingBuffer, TransferOptions};
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::time::mhz;
use embassy_stm32::usb::Driver;
use embassy_stm32::{bind_interrupts, peripherals, usb, Config};
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::Builder;
use panic_halt as _;

bind_interrupts!(struct Irqs {
    USB_LP_CAN_RX0 => usb::InterruptHandler<peripherals::USB>;
    DMA1_CHANNEL1 => embassy_stm32::dma::InterruptHandler<peripherals::DMA1_CH1>;
});

const DMA_BUF_SIZE: usize = 512;
const NUM_CHANNELS: usize = 10;
const ADC1_DR_ADDR: usize = 0x5000_0040;
// Fixed: channels 0-9 (PA0, PA1, PA2, PA3, PF4, PC0, PC1, PC2, PC3, PF2)
const ADC_HW_CHANNELS: [u8; NUM_CHANNELS] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

const CMD_SET_RATE: u8 = 0xA0;
const CMD_SET_CHANNEL: u8 = 0xB0;
const CMD_GET_MASK: u8 = 0xB2;

const DATA_SYNC: [u8; 2] = [0xAA, 0x55];
const ACK_RATE: u8 = 0xA1;
const ACK_CHANNEL: u8 = 0xB1;
const ACK_MASK: u8 = 0xB3;

const DFU_MAGIC: u32 = 0xDEADBEEF;
const SYSTEM_BOOTLOADER_ADDR: u32 = 0x1FFFD800;

struct ChannelCfg {
    enabled: bool,
    sample_time: u8,
}

static mut CH_CFG: [ChannelCfg; NUM_CHANNELS] = [
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
    ChannelCfg { enabled: true, sample_time: 2 },
];

fn sample_time_from_u8(val: u8) -> u8 {
    match val {
        0..=7 => val,
        _ => 2,
    }
}

fn check_and_enter_bootloader() {
    unsafe {
        let bkp0r = 0x4000_2850 as *const u32;
        let magic = core::ptr::read_volatile(bkp0r);
        
        if magic == DFU_MAGIC {
            cortex_m::interrupt::disable();
            
            core::arch::asm!(
                "ldr sp, [{addr}]",
                "ldr pc, [{addr}, #4]",
                addr = in(reg) SYSTEM_BOOTLOADER_ADDR,
                options(noreturn)
            );
        }
    }
}

fn enter_dfu_mode() -> ! {
    unsafe {
        let bkp0r = 0x4000_2850 as *mut u32;
        core::ptr::write_volatile(bkp0r, DFU_MAGIC);
        
        cortex_m::peripheral::SCB::sys_reset();
    }
}

fn adc_write_sequence(sample_time: u8) {
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
        w.set_dmacfg(Dmacfg::CIRCULAR);  // Fixed: circular DMA for continuous streaming
        w.set_ovrmod(true);
    });
    
    pac::ADC1.sqr1().modify(|w| {
        w.set_l((NUM_CHANNELS as u8) - 1);
        for i in 0..4 {
            w.set_sq(i, ADC_HW_CHANNELS[i]);
        }
    });
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
    
    // Fixed: configure sample time for ALL channels (SMPR1: ch 1-9, SMPR2: ch 0)
    pac::ADC1.smpr1().modify(|w| {
        for ch in 1..=9 {
            w.set_smp(ch, st);
        }
    });
    pac::ADC1.smpr2().modify(|w| {
        w.set_smp(0, st);  // Channel 0 is in SMPR2
    });
    
    pac::ADC1.cr().modify(|w| w.set_adstart(true));
}

fn adc_stop() {
    use embassy_stm32::pac;
    pac::ADC1.cr().modify(|w| w.set_adstp(true));
    while pac::ADC1.cr().read().adstp() {}
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    check_and_enter_bootloader();
    
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
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
        // Fixed: configure USB clock divisor through RCC config instead of raw register
        config.rcc.usb_clock_div = UsbClockDiv::DIV1_5;
    }

    let p = embassy_stm32::init(config);
  
    let mut led = Output::new(p.PE8, Level::Low, Speed::Low);
    
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
    
    adc_write_sequence(2);
    
    // Fixed: buffer size is a multiple of channel count for clean alignment
    static mut DMA_BUF: [u16; DMA_BUF_SIZE] = [0u16; DMA_BUF_SIZE];
    let mut ring_buf = unsafe {
        ReadableRingBuffer::new(
            Channel::new(p.DMA1_CH1, Irqs),
            (),
            ADC1_DR_ADDR as *mut u16,
            &mut *(&raw mut DMA_BUF),
            TransferOptions::default(),
        )
    };
    ring_buf.set_alignment(NUM_CHANNELS);
    ring_buf.start();
    
    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);
    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("Embassy");
    usb_config.product = Some("STM32F3 Oscilloscope");
    usb_config.serial_number = Some("001");
    
    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
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
    
    let mut class = CdcAcmClass::new(&mut builder, &mut state, 64);
    let mut usb = builder.build();
    
    let usb_fut = usb.run();
    
    let app_fut = async {
        loop {
            class.wait_connection().await;
            
            let mut scan_buf = [0u16; NUM_CHANNELS];
            let mut pkt = [0u8; 64];
            
            loop {
                let n = ring_buf.read_latest(&mut scan_buf);
                if n >= NUM_CHANNELS {
                    let header_len = 3usize;
                    let bytes_per_sample = 2usize;
                    let max_samples = (64 - header_len) / bytes_per_sample;
                    
                    // Fixed: sync marker format [0xAA, 0x55, <count>, <data...>]
                    // count is placed AFTER the sync bytes so neither is overwritten
                    pkt[0] = DATA_SYNC[0];
                    pkt[1] = DATA_SYNC[1];
                    
                    let mut idx = header_len;
                    let mut count = 0usize;
                    
                    for ch in 0..NUM_CHANNELS {
                        if count >= max_samples {
                            break;
                        }
                        if unsafe { CH_CFG[ch].enabled } {
                            let val = scan_buf[ch];
                            pkt[idx] = val as u8;
                            pkt[idx + 1] = (val >> 8) as u8;
                            idx += bytes_per_sample;
                            count += 1;
                        }
                    }
                    
                    pkt[2] = count as u8;  // count is at index 2, sync bytes at 0,1 are safe

                    if class.write_packet(&pkt[..idx]).await.is_err() {
                        break;
                    }

                    // Fixed: LED toggles only when data was successfully sent
                    led.toggle();
                }
                
                let mut cmd_buf = [0u8; 64];
                match class.read_packet(&mut cmd_buf).await {
                    Ok(n) if n >= 1 => {
                        match cmd_buf[0] {
                            CMD_SET_RATE if n >= 2 => {
                                let level = cmd_buf[1].min(7);
                                adc_stop();
                                adc_write_sequence(level);
                                pkt[0] = ACK_RATE;
                                pkt[1] = level;
                                let _ = class.write_packet(&pkt[..2]).await;
                            }
                            CMD_SET_CHANNEL if n >= 4 => {
                                let ch = cmd_buf[1] as usize;
                                let enable = cmd_buf[2] != 0;
                                let st = cmd_buf[3].min(7);
                                
                                if ch < NUM_CHANNELS {
                                    unsafe {
                                        CH_CFG[ch].enabled = enable;
                                        CH_CFG[ch].sample_time = st;
                                    }
                                    pkt[0] = ACK_CHANNEL;
                                    pkt[1] = ch as u8;
                                    pkt[2] = enable as u8;
                                    pkt[3] = st;
                                    let _ = class.write_packet(&pkt[..4]).await;
                                }
                            }
                            CMD_GET_MASK => {
                                let mut mask_lo = 0u8;
                                let mut mask_hi = 0u8;
                                
                                for i in 0..NUM_CHANNELS {
                                    if unsafe { CH_CFG[i].enabled } {
                                        if i < 8 {
                                            mask_lo |= 1 << i;
                                        } else {
                                            mask_hi |= 1 << (i - 8);
                                        }
                                    }
                                }
                                
                                // Fixed: send all 4 bytes including mask_hi
                                pkt[0] = ACK_MASK;
                                pkt[1] = mask_lo;
                                pkt[2] = mask_hi;
                                let _ = class.write_packet(&pkt[..3]).await;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }

                Timer::after_millis(10).await;
            }
        }
    };
    
    embassy_futures::join::join(usb_fut, app_fut).await;
}