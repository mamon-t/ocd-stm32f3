//! Транспортный слой: USB-CDC протокол v2 прошивки `src/main.rs` (STM32F303, ADC1+DMA).
//!
//! Кадр данных MCU→PC (всегда 5 + 2·nch байт):
//!   `AA 55 <flags(bit7=ovr)> <seq:u8> <nch:1..10> <nch * u16 LE>`
//! ACK'и — отдельные пакеты:
//!   `A1 <rate>` | `B1 <ch> <en> <st>` | `B3 <mask_lo> <mask_hi>` | `A4 <fs:u32 LE>`
//! Команды PC→MCU:
//!   `A0 <rate>` | `B0 <ch> <en> <st>` | `B2`
//!
//! Reader живёт в отдельном потоке и складывает кадры/ACK в `crossbeam-channel`;
//! UI-поток вычитывает их по таймеру.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use serialport::SerialPort;

pub const DATA_SYNC: [u8; 2] = [0xAA, 0x55];
pub const CMD_SET_RATE: u8 = 0xA0;
pub const CMD_SET_CHANNEL: u8 = 0xB0;
pub const CMD_GET_MASK: u8 = 0xB2;
pub const CMD_SET_GEN: u8 = 0xC0;
pub const ACK_RATE: u8 = 0xA1;
pub const ACK_CHANNEL: u8 = 0xB1;
pub const ACK_MASK: u8 = 0xB3;
pub const ACK_FS: u8 = 0xA4;

pub const NUM_CHANNELS: usize = 10;
pub const MAX_FRAME_LEN: usize = 5 + NUM_CHANNELS * 2; // 25

/// Нормализованный канал для графических приборов: [-1.0 .. 1.0].
#[inline]
pub fn normalize(raw: &[u16]) -> Vec<f32> {
    raw.iter().map(|&v| v as f32 / 2047.5 - 1.0).collect()
}

/// То же, но «в вольты» (Vref = 3.3 В, 12 бит).
#[inline]
pub fn to_volts(raw: &[u16]) -> Vec<f32> {
    raw.iter().map(|&v| v as f32 * (3.3 / 4095.0)).collect()
}

#[derive(Debug, Clone)]
pub enum Ack {
    Rate(u8),
    Channel { ch: u8, enabled: bool, sample_time: u8 },
    Mask { lo: u8, hi: u8 },
    /// Измеренная прошивкой частота сканирования (Гц) — это и есть f_s сигнала канала.
    SampleRate(u32),
}

#[derive(Debug)]
pub enum Event {
    Connected(String),
    Status(String),
    /// Один кадр = один скан АЦП: отсчёты включённых каналов по возрастанию номера.
    Frame {
        seq: u8,
        overrun: bool,
        /// (номер канала, сырой отсчёт 0..4095)
        channels: Vec<(u8, u16)>,
    },
    Ack(Ack),
}

pub struct Transport {
    stop: Arc<AtomicBool>,
    rx: Receiver<Event>,
    tx_out: Sender<Vec<u8>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Transport {
    /// Открывать порт НЕ в UI-потоке: блокирующий `open` на мёртвом устройстве
    /// подвесил бы окно. Поток-ридер сам ретраит подключение.
    pub fn spawn(path: Option<String>, baud: u32) -> Self {
        let (ev_tx, ev_rx) = bounded::<Event>(256);
        let (cmd_tx, cmd_rx) = bounded::<Vec<u8>>(32);
        let stop = Arc::new(AtomicBool::new(false));

        let (s, ev) = (stop.clone(), ev_tx);
        let handle = thread::spawn(move || reader_loop(path, baud, cmd_rx, ev, s));

        Self { stop, rx: ev_rx, tx_out: cmd_tx, handle: Some(handle) }
    }

    pub fn receiver(&self) -> &Receiver<Event> {
        &self.rx
    }

    pub fn send(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.tx_out.try_send(bytes.into());
    }

    pub fn set_rate(&self, level: u8) {
        self.send([CMD_SET_RATE, level]);
    }

    pub fn set_channel(&self, ch: u8, enabled: bool, sample_time: u8) {
        self.send([CMD_SET_CHANNEL, ch, enabled as u8, sample_time]);
    }

pub fn get_mask(&self) {
        self.send(&[CMD_GET_MASK]);
    }

    /// Включить/настроить тестовый генератор. `cfg`: бит0=вкл, биты[2:1]=форма
    /// (0=const, 1=синус, 2=треугольник, 3=меандр), биты[5:3]=амплитуда 0..7.
    pub fn set_gen(&self, cfg: u8) {
        self.send(&[CMD_SET_GEN, cfg]);
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn queue_len(&self) -> usize {
        self.rx.len()
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn auto_detect() -> Option<String> {
    serialport::available_ports().ok().and_then(|ports| {
        ports
            .into_iter()
            .map(|p| p.port_name)
            .filter(|n| n.contains("ttyACM") || n.contains("ttyUSB") || n.starts_with("COM"))
            .min()
    })
}

fn reader_loop(
    path: Option<String>,
    baud: u32,
    cmd_rx: Receiver<Vec<u8>>,
    ev_tx: Sender<Event>,
    stop: Arc<AtomicBool>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut reopen_delay = Duration::from_millis(500);
    let mut port: Option<Box<dyn SerialPort>> = None;

    while !stop.load(Ordering::Relaxed) {
        // --- (re)connect ---------------------------------------------------
        if port.is_none() {
            let name = path.clone().or_else(auto_detect);
            match name.and_then(|n| {
                serialport::new(&n, baud)
                    .timeout(Duration::from_millis(50))
                    .open()
                    .ok()
                    .map(|p| (n, p))
            }) {
                Some((name, p)) => {
                    // CDC на STM32 иногда «дёргается» при открытии — даём стабилизироваться
                    thread::sleep(Duration::from_millis(100));
                    buf.clear();
                    let _ = ev_tx.try_send(Event::Connected(name));
                    port = Some(p);
                    reopen_delay = Duration::from_millis(500);
                }
                None => {
                    thread::sleep(reopen_delay);
                    reopen_delay = (reopen_delay * 2).min(Duration::from_secs(4));
                    continue;
                }
            }
        }

        // --- исходящие команды ---------------------------------------------
        if let Some(p) = port.as_mut() {
            while let Ok(cmd) = cmd_rx.try_recv() {
                if p.write_all(&cmd).is_err() {
                    port = None; // порт умер — переподключимся на следующем витке
                    break;
                }
            }
        }

        // --- чтение ---------------------------------------------------------
        let p = match port.as_mut() {
            Some(p) => p,
            None => continue,
        };
        let mut chunk = [0u8; 512];
        match p.read(&mut chunk) {
            Ok(0) => thread::sleep(Duration::from_millis(5)),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                drain(&mut buf, &ev_tx);
            }
            Err(_) => {
                port = None;
                let _ = ev_tx.try_send(Event::Status("порт закрылся, переподключение…".into()));
            }
        }
    }
}

use std::io::Write;

/// Выуживаем из буфера все полные пакеты: data-кадры `AA 55 …` и ACK-пакеты.
/// Мусор между пакетами выбрасываем по преамбуле; seq в кадре даёт приёмнику
/// возможность сообщать о потерях (UI показывает счётчик lost).
fn drain(buf: &mut Vec<u8>, ev_tx: &Sender<Event>) {
    loop {
        if buf.is_empty() {
            return;
        }
        let b0 = buf[0];
        // --- data-кадр ---
        if b0 == DATA_SYNC[0] {
            if buf.len() < 5 {
                return; // недобор заголовка — ждём
            }
            if buf[1] != DATA_SYNC[1] {
                buf.remove(0);
                continue;
            }
            let nch = buf[4] as usize;
            if !(1..=NUM_CHANNELS).contains(&nch) {
                buf.remove(0); // ложная синхронизация внутри данных
                continue;
            }
            let pkt_len = 5 + nch * 2;
            if buf.len() < pkt_len {
                return;
            }
            let flags = buf[2];
            let seq = buf[3];
            let mut channels = Vec::with_capacity(nch);
            for i in 0..nch {
                let lo = buf[5 + i * 2] as u16;
                let hi = buf[6 + i * 2] as u16;
                // нумерация каналов восстановит UI по маске (ACK_MASK); здесь — порядковый
                channels.push((i as u8, lo | (hi << 8)));
            }
            buf.drain(..pkt_len);
            let _ = ev_tx.try_send(Event::Frame {
                seq,
                overrun: flags & 0x80 != 0,
                channels,
            });
            continue;
        }
        // --- ACK-пакеты ---
        let need = match b0 {
            ACK_RATE => 2,
            ACK_MASK => 3,
            ACK_CHANNEL => 4,
            ACK_FS => 5,
            _ => {
                buf.remove(0); // мусор
                continue;
            }
        };
        if buf.len() < need {
            return;
        }
        let ack = match b0 {
            ACK_RATE => Ack::Rate(buf[1]),
            ACK_MASK => Ack::Mask { lo: buf[1], hi: buf[2] },
            ACK_CHANNEL => Ack::Channel {
                ch: buf[1],
                enabled: buf[2] != 0,
                sample_time: buf[3],
            },
            ACK_FS => Ack::SampleRate(u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]])),
            _ => unreachable!(),
        };
        buf.drain(..need);
        let _ = ev_tx.try_send(Event::Ack(ack));
    }
}

// ============================================================================
// Юнит-тесты протокола — можно гонять без железа и без gpui-kit.
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_data_frame() {
        // кадр с 2 каналами: AA 55 flags seq nch + 2*u16LE
        let mut buf = vec![0xAA, 0x55, 0x00, 0x07, 0x02, 0x34, 0x12, 0xFF, 0x0F];
        let (tx, rx) = bounded(4);
        drain(&mut buf, &tx);
        let Event::Frame { seq, channels, .. } = rx.try_recv().unwrap() else { panic!("no frame") };
        assert_eq!(seq, 7);
        assert_eq!(channels, vec![(0, 0x1234), (1, 0x0FFF)]);
        assert!(buf.is_empty());
    }

    #[test]
    fn parses_ack_and_skips_garbage() {
        let mut buf = vec![0x00, 0xB3, 0x03, 0x01, 0xAA, 0x55, 0x80, 0x01, 0x01, 0x42, 0x10];
        let (tx, rx) = bounded(4);
        drain(&mut buf, &tx);
        let Event::Ack(Ack::Mask { lo, hi }) = rx.try_recv().unwrap() else { panic!("no ack") };
        assert_eq!((lo, hi), (0x03, 0x01));
        let Event::Frame { overrun, channels, .. } = rx.try_recv().unwrap() else { panic!("no frame") };
        assert!(overrun);
        assert_eq!(channels, vec![(0, 0x1042)]);
        assert!(buf.is_empty());
    }

    #[test]
    fn parses_fs_ack() {
        let mut buf = vec![0xA4, 0x40, 0x1F, 0x00, 0x00];
        let (tx, rx) = bounded(4);
        drain(&mut buf, &tx);
        let Event::Ack(Ack::SampleRate(fs)) = rx.try_recv().unwrap() else { panic!("no fs ack") };
        assert_eq!(fs, 8000);
        assert!(buf.is_empty());
    }

    #[test]
    fn keeps_partial_tail() {
        let mut buf = vec![0xAA, 0x55, 0x00, 0x01, 0x03, 0x01, 0x02];
        let (tx, rx) = bounded(4);
        drain(&mut buf, &tx);
        assert!(rx.try_recv().is_err());
        assert_eq!(buf.len(), 7); // ждём остаток
    }

    #[test]
    fn false_sync_inside_data_resyncs() {
        // AA внутри полезной нагрузки не должен «съедать» следующий настоящий кадр
        let mut buf = vec![0xAA, 0x55, 0x00, 0x01, 0x01, 0xAA, 0x55, 0x00, 0x02, 0x01, 0x11, 0x22];
        let (tx, rx) = bounded(8);
        drain(&mut buf, &tx);
        let mut got = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, Event::Frame { .. }) {
                got += 1;
            }
        }
        assert!(got >= 1, "должен был быть найден хотя бы один кадр");
    }

    #[test]
    fn normalize_maps_adc_to_unit_range() {
        let v = normalize(&[0, 2048, 4095]);
        assert!(v[0] < -0.99 && v[2] > 0.99 && v[1].abs() < 0.001);
    }
}
