//! LabBench — gpui-kit-фронтенд измерительного комплекса (gpui-kit 0.7.1)
//! Вкладки: Oscilloscope · Spectrum · LCR · Transient (Z-метр).

mod canvas;
mod dsp;
mod transport;

use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// --- Явные импорты для gpui-kit 0.7.1 ---
use gpui_kit::base::Selectable;
use gpui_kit::component::{
    button::Button, h_flex, label::Label, separator::Separator, v_flex,
};
use gpui_kit::*;

use serde::Deserialize;

use canvas::{channel_color, PlotCanvas, TraceCanvas, UNIT_V};
use transport::Transport;

/// Максимум точек на канал в кольцевом буфере осциллографа.
const RING_LEN: usize = 4096;
/// Частота дискретизации симулятора.
const SIM_FS: f32 = 72_000.0;
/// Период опроса транспорта/симулятора в UI-потоке.
const PUMP_INTERVAL: Duration = Duration::from_millis(33);

// --- Палитра GUI: строгий «серо-стальной» приборный стиль ---
const C_BG: u32 = 0x14171c; // фон окна
const C_PANEL: u32 = 0x2a2f37; // панели (серо-сталь)
const C_PANEL_HI: u32 = 0x343b45; // панели, верхний уровень
const C_BORDER: u32 = 0x454c57; // разделители/границы
const C_TEXT: u32 = 0xe6e9ee; // контрастный текст
const C_MUTED: u32 = 0x99a3ae; // приглушённый текст
const C_ACCENT: u32 = 0x4d9de0; // акцентный синий
const C_OFF: u32 = 0x555d68; // «выключено» (серый)
const C_SEL: u32 = 0x34465c; // фон выбранного канала

/// Шаг сетки развёртки (мс/дел), от 10 мкс до 5 мс.
const TIME_DIVS: [f32; 9] = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0];
/// Шаг сетки по вертикали (В/дел), от 50 мВ до 10 В.
const V_DIVS: [f32; 8] = [0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0];

/// Индекс ближайшего значения в таблице делений.
fn nearest_idx(arr: &[f32], cur: f32) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for (i, &v) in arr.iter().enumerate() {
        let d = (v - cur).abs();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// Счётчик тактов pump для периодической диагностики (раз в ~1 c).
static PUMP_TICKS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Debug, Deserialize)]
enum Tab {
    Scope,
    Spectrum,
    Lcr,
    Transient,
}

/// Секция левой панели; открыта максимум одна (аккордеон).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PanelSection {
    Channels,
    Timebase,
    Sync,
    Cursors,
}

pub struct LabApp {
    transport: Option<Transport>,
    simulate: bool,
    sim_phase: f64,
    tab: Tab,

    /// Кольцевые буферы 10 каналов.
    rings: Vec<Vec<f32>>,
    write_pos: usize,
    frame_count: u64,
    fps: f32,
    last_fps_check: Instant,
    frames_since_check: u32,

    // --- настройки scope ---
    rate_level: u8,
    ch_enabled: [bool; 10],
    trigger_on: bool,
    time_div_ms: f32,
    /// Вертикальный масштаб, вольт на деление.
    v_per_div: f32,

    // --- состояние устройства ---
    last_seq: Option<u8>,
    lost_frames: u64,

    // --- результаты DSP ---
    spec_freqs: Rc<Vec<f32>>,
    spec_dbs: Rc<Vec<f32>>,
    dev_mask: u16,
    /// Текущая частота сканирования устройства (из Ack::SampleRate).
    fs_hz: f32,
    lcr_result: Option<(dsp::Impedance, f64, &'static str, f32)>,
    transient_result: Option<(f32, Rc<Vec<f32>>)>,

    // --- тестовый генератор прошивки (DAC1, команда 0xC0) ---
    gen_en: bool,
    gen_shape: u8, // 1=синус, 2=треугольник, 3=меандр
    gen_amp: u8,   // 1..8, уровень амплитуды

    // --- измерения / курсоры / спектр / смещение ---
    /// Активный канал для таблицы измерений, авто-развёртки и смещения.
    selected_ch: usize,
    cursors_on: bool,
    /// Курсоры A/B как доли ширины окна [0..1].
    cursor_a: f32,
    cursor_b: f32,
    spec_window: dsp::WindowKind,
    /// Экспоненциальное усреднение спектра (снижает шум).
    spec_avg: bool,
    spec_avg_dbs: Vec<f32>,
    /// Найденный пик спектра: (частота Гц, уровень дБ).
    spec_peak: Option<(f32, f32)>,
    /// Фиксированная шкала спектра −120..0 дБ (иначе — авто).
    spec_fixed_db: bool,
    /// Вертикальное смещение канала, вольт.
    ch_offset_v: [f32; 10],
    /// Открытая секция левой панели (аккордеон).
    open_section: Option<PanelSection>,

    status_line: String,
}

impl LabApp {
    pub fn new(
        port: Option<String>,
        baud: u32,
        simulate: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let transport = if simulate && port.is_none() {
            None
        } else {
            Some(Transport::spawn(port, baud))
        };

        let mut app = Self {
            transport,
            simulate,
            sim_phase: 0.0,
            tab: Tab::Scope,
            rings: (0..10).map(|_| vec![0.0; RING_LEN]).collect(),
            write_pos: 0,
            frame_count: 0,
            fps: 0.0,
            last_fps_check: Instant::now(),
            frames_since_check: 0,
            rate_level: 3,
            ch_enabled: [true, true, false, false, false, false, false, false, false, false],
            trigger_on: true,
            time_div_ms: 1.0,
            v_per_div: 0.5,
            last_seq: None,
            lost_frames: 0,
            spec_freqs: Rc::new(vec![]),
            spec_dbs: Rc::new(vec![]),
            dev_mask: 0x03,
            fs_hz: if simulate { SIM_FS } else { 0.0 },
            lcr_result: None,
            transient_result: None,
            gen_en: false,
            gen_shape: 1,
            gen_amp: 4,
            selected_ch: 0,
            cursors_on: false,
            cursor_a: 0.35,
            cursor_b: 0.65,
            spec_window: dsp::WindowKind::Hann,
            spec_avg: false,
            spec_avg_dbs: Vec::new(),
            spec_peak: None,
            spec_fixed_db: false,
            ch_offset_v: [0.0; 10],
            open_section: Some(PanelSection::Channels),
            status_line: "ожидание данных…".to_string(),
        };
        app.start_pump(cx);
        app
    }

    /// Фоновый такт: раз в `PUMP_INTERVAL` вычитывает события транспорта,
    /// крутит симулятор и перерисовывает окно.
    fn start_pump(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(PUMP_INTERVAL).await;
            if this.upgrade().is_none() {
                break;
            }
            let _ = this.update(cx, |app, cx| app.pump(cx));
        })
        .detach();
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut got_any = false;
        let mut events = Vec::new();

        if let Some(t) = &self.transport {
            while let Ok(ev) = t.receiver().try_recv() {
                events.push(ev);
            }
        }

        let n_events = events.len();
        for ev in events {
            got_any = true;
            match ev {
                transport::Event::Connected(name) => {
                    self.status_line = format!("подключено: {name}");
                    self.last_seq = None;
                    if let Some(t_again) = &self.transport {
                        t_again.get_mask();
                    }
                    cx.notify();
                }
                transport::Event::Status(s) => {
                    self.status_line = s.to_string();
                    cx.notify();
                }
                transport::Event::Frame { seq, overrun, channels } => {
                    if let Some(last) = self.last_seq {
                        let delta = seq.wrapping_sub(last);
                        if delta > 0 {
                            self.lost_frames += (delta - 1) as u64;
                        }
                    }
                    self.last_seq = Some(seq);
                    if overrun {
                        self.status_line = "ADC/DMA overrun на устройстве".to_string();
                    }
                    self.push_frame(&channels);
                }
                transport::Event::Ack(ack) => {
                    use transport::Ack;
                    match ack {
                        Ack::Rate(level) => {
                            self.rate_level = level;
                            self.status_line = format!("применён rate={level}");
                        }
                        Ack::SampleRate(fs) => {
                            self.fs_hz = fs as f32;
                            self.status_line = format!("fs = {:.0} Гц", self.fs_hz);
                        }
                        Ack::Channel { ch, enabled, .. } => {
                            if (ch as usize) < 10 {
                                self.ch_enabled[ch as usize] = enabled;
                                self.dev_mask &= !(1 << ch);
                                if enabled {
                                    self.dev_mask |= 1 << ch;
                                }
                            }
                        }
                        Ack::Mask { lo, hi } => {
                            self.dev_mask = (lo as u16) | ((hi as u16) << 8);
                            for c in 0..10 {
                                self.ch_enabled[c] = self.dev_mask & (1 << c) != 0;
                            }
                        }
                    }
                    cx.notify();
                }
            }
        }

        // Симулятор
        if !got_any && self.simulate {
            let n = 128;
            for i in 0..n {
                let t = (self.sim_phase + i as f64 / SIM_FS as f64) as f32;
                let v0 = 0.7
                    * ((std::f64::consts::TAU * 1000.0 * t as f64).sin() as f32)
                    + 0.25 * ((std::f64::consts::TAU * 5000.0 * t as f64).sin() as f32);
                let v1 = 0.4 * ((std::f64::consts::TAU * 300.0 * t as f64).cos() as f32);

                let k = ((self.sim_phase * SIM_FS as f64) as usize + i) % 8192;
                let v_step = 1.0 - (-(k as f32) / (SIM_FS * 1e-3)).exp();

                self.rings[0][k % RING_LEN] = v0.max(-1.0).min(1.0);
                self.rings[1][k % RING_LEN] = (v1 + noise(k)).max(-1.0).min(1.0);
                self.rings[2][k % RING_LEN] = v_step * 2.0 - 1.0;
            }
            self.sim_phase += n as f64 / SIM_FS as f64;
            got_any = true;
        }

        if got_any {
            self.frame_count += 1;
            self.frames_since_check += 1;
            let dt = self.last_fps_check.elapsed();
            if dt >= Duration::from_millis(500) {
                self.fps = self.frames_since_check as f32 / dt.as_secs_f32();
                self.frames_since_check = 0;
                self.last_fps_check = Instant::now();
                self.recompute_dsp();
            }
            cx.notify();
        }

        let tick = PUMP_TICKS.fetch_add(1, Ordering::Relaxed);
        if tick % 30 == 0 {
            eprintln!(
                "[labbench] pump tick={} frame_count={} events/tick={} got_any={} queue_len={} transport={} status={:?}",
                tick,
                self.frame_count,
                n_events,
                got_any,
                self.transport.as_ref().map(|t| t.queue_len()).unwrap_or(0),
                self.transport.is_some(),
                self.status_line,
            );
        }
    }

    fn push_frame(&mut self, channels: &[(u8, u16)]) {
        let mut slot = 0usize;
        for ch in 0..10 {
            if self.dev_mask & (1 << ch) == 0 {
                continue;
            }
            if slot >= channels.len() {
                break;
            }
            let raw = channels[slot].1;
            self.rings[ch][self.write_pos] = raw as f32 / 2047.5 - 1.0;
            slot += 1;
        }
        self.write_pos = (self.write_pos + 1) % RING_LEN;
    }

    fn channel_slice(&self, ch: usize) -> Rc<Vec<f32>> {
        let r = &self.rings[ch];
        let take = r.len().min(RING_LEN);
        let mut out = Vec::with_capacity(take);
        for i in 0..take {
            let idx = (self.write_pos + r.len() - 1 - i) % r.len();
            out.push(r[idx]);
        }
        out.reverse();
        Rc::new(out)
    }

    fn recompute_dsp(&mut self) {
        let fs = self.fs_hz;
        if fs <= 0.0 {
            return;
        }
        let y0 = self.channel_slice(self.selected_ch.min(9));
        if y0.len() >= 256 {
            let n = y0.len().next_power_of_two().min(y0.len());
            let win = self.spec_window.coeffs(n);
            let (fr, db) = dsp::amplitude_db(&y0[..n], fs, &win);
            self.spec_peak = dsp::spectral_peak(&fr, &db);
            // Экспоненциальное усреднение по бинам (снижение шумовой дорожки).
            let out_db: Vec<f32> = if self.spec_avg {
                const ALPHA: f32 = 0.25;
                if self.spec_avg_dbs.len() != db.len() {
                    self.spec_avg_dbs = db.clone();
                } else {
                    for (acc, &d) in self.spec_avg_dbs.iter_mut().zip(db.iter()) {
                        *acc += ALPHA * (d - *acc);
                    }
                }
                self.spec_avg_dbs.clone()
            } else {
                self.spec_avg_dbs.clear();
                db
            };
            self.spec_freqs = Rc::new(fr);
            self.spec_dbs = Rc::new(out_db);
        }

        let y1 = self.channel_slice(1);
        let current_y0 = self.channel_slice(0);
        let n = current_y0.len().min(y1.len()).min(4096);
        if n >= 256 {
            let f_test = 5_000.0f32;
            if fs > 2.0 * f_test {
                let z = dsp::measure_z(&current_y0[..n], &y1[..n], f_test, fs, 100.0);
                let (val, unit) = dsp::lcr(z, f_test);
                self.lcr_result = Some((z, val, unit, f_test));
            } else {
                self.lcr_result = None;
            }
        }

        let y2 = self.channel_slice(2);
        if y2.len() >= 256 {
            let tr = dsp::rise_time_10_90(&y2, fs);
            self.transient_result = Some((tr.unwrap_or(0.0), y2.clone()));
        }
    }

    fn visible_channels(&self) -> Vec<Rc<Vec<f32>>> {
        // Окно отображения: 10 делений по `time_div_ms`.
        let fs = self.fs_hz.max(1.0);
        let win = ((self.time_div_ms * 10.0) * fs / 1000.0).round() as usize;
        let win = win.clamp(16, RING_LEN);
        let mut out = Vec::new();
        for ch in 0..10 {
            if self.ch_enabled[ch] {
                let raw = self.channel_slice(ch);
                let raw = if raw.len() > win {
                    Rc::new(raw[raw.len() - win..].to_vec())
                } else {
                    raw
                };
                let data = if self.trigger_on && out.is_empty() {
                    match dsp::find_trigger(&raw, 0.0, 0.05) {
                        Some(idx) => Rc::new(dsp::roll_to(&raw, idx)),
                        None => raw,
                    }
                } else {
                    raw
                };
                out.push(data);
            }
        }
        if out.is_empty() {
            out.push(self.channel_slice(0));
        }
        out
    }

    /// Сдвинуть горизонтальную развёртку на один шаг (dir: −1 медленнее, +1 быстрее).
    fn step_time_div(&mut self, dir: i32) {
        let i = nearest_idx(&TIME_DIVS, self.time_div_ms) as i32;
        let ni = (i + dir).clamp(0, TIME_DIVS.len() as i32 - 1) as usize;
        self.time_div_ms = TIME_DIVS[ni];
    }

    /// Сдвинуть вертикальный делитель на один шаг (dir: −1 меньше В/дел, +1 больше).
    fn step_v_div(&mut self, dir: i32) {
        let i = nearest_idx(&V_DIVS, self.v_per_div) as i32;
        let ni = (i + dir).clamp(0, V_DIVS.len() as i32 - 1) as usize;
        self.v_per_div = V_DIVS[ni];
    }

    /// Шаг частоты выборки устройства (0..7 — время выборки АЦП, команда 0xA0).
    fn step_rate(&mut self, dir: i32) {
        let ni = (self.rate_level as i32 + dir).clamp(0, 7) as u8;
        self.rate_level = ni;
        if let Some(t) = &self.transport {
            t.set_rate(ni);
        }
    }

    /// Смещение трассы активного канала на ±0.5 В.
    fn step_offset(&mut self, dir: i32) {
        let v = self.ch_offset_v[self.selected_ch] + dir as f32 * 0.5;
        self.ch_offset_v[self.selected_ch] = v.clamp(-5.0, 5.0);
    }

    /// Авто-развёртка: подбор мс/дел и В/дел под сигнал активного канала.
    fn auto_set(&mut self) {
        let y = self.displayed_slice(self.selected_ch);
        if let Some(m) = dsp::measure_channel(&y, self.fs_hz.max(1.0), UNIT_V) {
            if m.freq_hz > 1.0 {
                // ~3 периода на 10 делений.
                let td = (3.0 / m.freq_hz * 1000.0) / 10.0;
                self.time_div_ms = TIME_DIVS[nearest_idx(&TIME_DIVS, td)];
            }
            if m.vpp > 1e-3 {
                self.v_per_div = V_DIVS[nearest_idx(&V_DIVS, m.vpp / 6.0)];
            }
        }
    }

    /// Последние `win` отсчётов канала — то же окно, что рисуется на экране.
    fn displayed_slice(&self, ch: usize) -> Vec<f32> {
        let fs = self.fs_hz.max(1.0);
        let win = (((self.time_div_ms * 10.0) * fs / 1000.0).round() as usize).clamp(16, RING_LEN);
        let raw = self.channel_slice(ch.min(9));
        if raw.len() > win {
            raw[raw.len() - win..].to_vec()
        } else {
            raw.to_vec()
        }
    }

    /// Измерения активного канала за текущее окно.
    fn selected_metrics(&self) -> Option<dsp::ChanMetrics> {
        dsp::measure_channel(&self.displayed_slice(self.selected_ch), self.fs_hz.max(1.0), UNIT_V)
    }

    /// ΔT / 1/ΔT / ΔV между курсорами (по активному каналу).
    fn cursor_readout(&self) -> Option<(f32, f32, f32)> {
        if !self.cursors_on {
            return None;
        }
        let y = self.displayed_slice(self.selected_ch);
        if y.len() < 2 {
            return None;
        }
        let ia = (self.cursor_a.clamp(0.0, 1.0) * (y.len() - 1) as f32).round() as usize;
        let ib = (self.cursor_b.clamp(0.0, 1.0) * (y.len() - 1) as f32).round() as usize;
        let dv = (y[ib] - y[ia]) * UNIT_V;
        let dt_ms = (self.cursor_b - self.cursor_a).abs() * self.time_div_ms * 10.0;
        let f_hz = if dt_ms > 1e-9 { 1000.0 / dt_ms } else { 0.0 };
        Some((dt_ms, f_hz, dv))
    }

    fn render_scope(&self, _cx: &Context<Self>) -> AnyElement {
        let chans = self.visible_channels();
        let ids: Vec<usize> = (0..10).filter(|&c| self.ch_enabled[c]).collect();
        let offsets: Vec<f32> = if ids.is_empty() {
            vec![self.ch_offset_v[0]]
        } else {
            ids.iter().map(|&c| self.ch_offset_v[c]).collect()
        };
        let highlight = if ids.is_empty() {
            Some(0)
        } else {
            ids.iter().position(|&c| c == self.selected_ch)
        };

        let mut header = h_flex()
            .w_full()
            .gap_4()
            .items_center()
            .child(Label::new(format!("{:.1} мс/дел", self.time_div_ms)).text_color(rgb(C_TEXT)))
            .child(
                Label::new(if self.trigger_on { "триггер: вкл" } else { "триггер: выкл" })
                    .text_color(rgb(C_MUTED)),
            )
            .child(
                Label::new(format!("канал CH{}", self.selected_ch)).text_color(rgb(C_MUTED)),
            );
        if let Some((dt, f, dv)) = self.cursor_readout() {
            let dt_s = if dt < 1.0 {
                format!("{:.0} мкс", dt * 1000.0)
            } else {
                format!("{dt:.2} мс")
            };
            let f_s = if f >= 1000.0 {
                format!("{:.2} кГц", f / 1000.0)
            } else {
                format!("{f:.0} Гц")
            };
            header = header
                .child(Label::new(format!("ΔT {dt_s}")).text_color(rgb(C_ACCENT)))
                .child(Label::new(format!("1/ΔT {f_s}")).text_color(rgb(C_ACCENT)))
                .child(Label::new(format!("ΔV {dv:+.2} В")).text_color(rgb(C_ACCENT)));
        }
        header = header.child(div().flex_1()).child(
            Label::new(format!("каналов: {}", chans.len())).text_color(rgb(C_MUTED)),
        );
        if self.lost_frames > 0 {
            header = header.child(
                Label::new(format!("потеряно {}", self.lost_frames)).text_color(rgb(C_ACCENT)),
            );
        }

        h_flex()
            .size_full()
            .items_stretch()
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .gap_2()
                    .child(header)
                    .child(
                        div()
                            .id("scope-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                TraceCanvas::new(chans)
                                    .trigger_level(if self.trigger_on {
                                        Some(0.0)
                                    } else {
                                        None
                                    })
                                    .v_per_div(self.v_per_div)
                                    .time_per_div_ms(self.time_div_ms)
                                    .offsets(offsets)
                                    .highlight(highlight)
                                    .cursors(if self.cursors_on {
                                        Some((self.cursor_a, self.cursor_b))
                                    } else {
                                        None
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_spectrum(&self, cx: &mut Context<Self>) -> AnyElement {
        let points: Vec<(f32, f32)> = self
            .spec_freqs
            .iter()
            .zip(self.spec_dbs.iter())
            .map(|(&f, &db)| (f, db))
            .collect();
        let x_fmt: Rc<dyn Fn(f32) -> String> = Rc::new(|f: f32| {
            if f >= 1000.0 {
                format!("{:.0}k", f / 1000.0)
            } else {
                format!("{f:.0}")
            }
        });
        let y_fmt: Rc<dyn Fn(f32) -> String> = Rc::new(|db: f32| format!("{db:.0}"));

        let mut wrow = h_flex()
            .gap_1()
            .items_center()
            .child(Label::new("ОКНО").text_color(rgb(C_MUTED)));
        for w in [
            dsp::WindowKind::Hann,
            dsp::WindowKind::Hamming,
            dsp::WindowKind::Blackman,
            dsp::WindowKind::Rect,
        ] {
            wrow = wrow.child(
                Button::new(format!("spec-win-{w:?}"))
                    .label(w.name())
                    .tooltip(w.name())
                    .compact()
                    .selected(self.spec_window == w)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.spec_window = w;
                        this.spec_avg_dbs.clear();
                        cx.notify();
                    })),
            );
        }

        let mut header = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(Label::new("СПЕКТР").text_color(rgb(C_TEXT)))
            .child(div().w(px(1.)).h(px(18.)).bg(rgb(C_BORDER)))
            .child(wrow)
            .child(
                Button::new("spec-avg")
                    .label("усредн.")
                    .tooltip("Экспоненциальное усреднение спектра")
                    .compact()
                    .selected(self.spec_avg)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.spec_avg = !this.spec_avg;
                        this.spec_avg_dbs.clear();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("spec-scale")
                    .label(if self.spec_fixed_db { "дБ: −120…0" } else { "дБ: авто" })
                    .compact()
                    .selected(self.spec_fixed_db)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.spec_fixed_db = !this.spec_fixed_db;
                        cx.notify();
                    })),
            )
            .child(div().flex_1());
        if let Some((f, db)) = self.spec_peak {
            header = header.child(
                Label::new(format!("пик {:.0} Гц / {db:.1} дБ", f)).text_color(rgb(C_ACCENT)),
            );
        }
        header = header
            .child(Label::new(format!("fs = {:.0} Гц", self.fs_hz)).text_color(rgb(C_MUTED)));

        let mut plot = PlotCanvas::new(points, x_fmt, y_fmt).line_color(rgb(C_ACCENT).into());
        if self.spec_fixed_db {
            plot = plot.y_range(-120.0, 0.0);
        }
        if let Some((f, _)) = self.spec_peak {
            plot = plot.marker_x(f);
        }

        h_flex()
            .size_full()
            .items_stretch()
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .gap_2()
                    .child(header)
                    .child(
                        div()
                            .id("spectrum-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(plot),
                    ),
            )
            .into_any_element()
    }

    fn render_lcr(&self, _cx: &Context<Self>) -> AnyElement {
        let header = h_flex()
            .w_full()
            .gap_4()
            .items_center()
            .child(Label::new("LCR-МЕТР").text_color(rgb(C_TEXT)))
            .child(div().flex_1())
            .child(Label::new(format!("fs = {:.0} Гц", self.fs_hz)).text_color(rgb(C_MUTED)));

        let body = match self.lcr_result {
            Some((z, val, unit, freq)) => v_flex()
                .items_start()
                .gap_3()
                .child(
                    Label::new(format!("Частота теста: {freq:.0} Гц")).text_color(rgb(C_MUTED)),
                )
                .child(Separator::horizontal())
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(Label::new("Значение:").text_color(rgb(C_MUTED)))
                        .child(Label::new(format!("{val:.2} {unit}")).text_color(rgb(C_TEXT))),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(Label::new("|Z|:").text_color(rgb(C_MUTED)))
                        .child(Label::new(format!("{:.1} Ом", z.mag())).text_color(rgb(C_TEXT))),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(Label::new("Фаза:").text_color(rgb(C_MUTED)))
                        .child(
                            Label::new(format!("{:+.1}° (R + jX)", z.phase_deg()))
                                .text_color(rgb(C_TEXT)),
                        ),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(Label::new("R + jX:").text_color(rgb(C_MUTED)))
                        .child(
                            Label::new(format!("{:.3} + j{:.3} Ом", z.re, z.im))
                                .text_color(rgb(C_TEXT)),
                        ),
                )
                .child(Label::new(&self.status_line).text_color(rgb(C_MUTED))),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("Ожидание данных LCR…").text_color(rgb(C_MUTED))),
        };

        v_flex()
            .size_full()
            .gap_3()
            .child(header)
            .child(div().flex_1().w_full().child(body))
            .into_any_element()
    }

    fn render_transient(&self, _cx: &Context<Self>) -> AnyElement {
        let (tr, data) = match &self.transient_result {
            Some(r) => (r.0, r.1.clone()),
            None => {
                return h_flex()
                    .size_full()
                    .child(
                        v_flex()
                            .items_center()
                            .justify_center()
                            .child(Label::new("Ожидание переходного процесса…")),
                    )
                    .into_any_element();
            }
        };

        let points: Vec<(f32, f32)> = data
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as f32, v))
            .collect();
        let fs = self.fs_hz.max(1.0);
        let x_fmt: Rc<dyn Fn(f32) -> String> = Rc::new(move |i: f32| {
            let us = i / fs * 1e6;
            if us >= 1000.0 {
                format!("{:.1}мс", us / 1000.0)
            } else {
                format!("{us:.0}мкс")
            }
        });
        let y_fmt: Rc<dyn Fn(f32) -> String> = Rc::new(|v: f32| format!("{:.2}", v * UNIT_V));

        h_flex()
            .size_full()
            .items_stretch()
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .gap_2()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_4()
                            .items_center()
                            .child(Label::new("ПЕРЕХОДНЫЙ ПРОЦЕСС").text_color(rgb(C_TEXT)))
                            .child(
                                Label::new(format!("Rise 10–90%: {tr:.3} мкс"))
                                    .text_color(rgb(C_ACCENT)),
                            )
                            .child(div().flex_1())
                            .child(
                                Label::new(format!("fs = {:.0} Гц", self.fs_hz))
                                    .text_color(rgb(C_MUTED)),
                            )
                            .child(
                                Label::new(format!("{:.0} fps", self.fps)).text_color(rgb(C_MUTED)),
                            ),
                    )
                    .child(
                        div()
                            .id("transient-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                PlotCanvas::new(points, x_fmt, y_fmt)
                                    .line_color(channel_color(2)),
                            ),
                    )
                    .child(Label::new(&self.status_line)),
            )
            .into_any_element()
    }

    /// Пиктограмма-вкладка; полное имя режима показывается в тулбаре.
    fn tab_button(&self, cx: &mut Context<Self>, glyph: &'static str, title: &'static str, tab: Tab) -> Button {
        Button::new(glyph)
            .label(glyph)
            .tooltip(title)
            .selected(self.tab == tab)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tab = tab;
                cx.notify();
            }))
    }

    /// Русское имя активного режима для заголовка тулбара.
    fn tab_title(&self) -> &'static str {
        match self.tab {
            Tab::Scope => "ОСЦИЛЛОГРАФ",
            Tab::Spectrum => "СПЕКТР",
            Tab::Lcr => "LCR-МЕТР",
            Tab::Transient => "ПЕРЕХОДНЫЙ ПРОЦЕСС",
        }
    }

    /// Размах сигнала канала в вольтах (по всему кольцевому буферу).
    fn channel_vpp(&self, ch: usize) -> f32 {
        let r = &self.rings[ch];
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for &v in r.iter() {
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        if lo.is_finite() && hi.is_finite() {
            (hi - lo) * UNIT_V
        } else {
            0.0
        }
    }

    /// Включить/выключить канал и передать маску устройству.
    fn toggle_channel(&mut self, ch: usize, cx: &mut Context<Self>) {
        let en = !self.ch_enabled[ch];
        self.ch_enabled[ch] = en;
        if en {
            self.dev_mask |= 1 << ch;
        } else {
            self.dev_mask &= !(1 << ch);
        }
        if let Some(t) = &self.transport {
            t.set_channel(ch as u8, en, 2);
        }
        cx.notify();
    }

    /// Верхняя панель: название прибора, вкладки, статус линии.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let link_ok = self.transport.is_some();
        let status = h_flex()
            .gap_4()
            .items_center()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(8.))
                            .h(px(8.))
                            .rounded(px(4.))
                            .bg(if link_ok { rgb(C_ACCENT) } else { rgb(C_BORDER) }),
                    )
                    .child(
                        Label::new(if link_ok { "USB: линия" } else { "USB: нет" })
                            .text_color(rgb(C_MUTED)),
                    ),
            )
            .child(Label::new(format!("fs {:.0} Гц", self.fs_hz)).text_color(rgb(C_MUTED)))
            .child(Label::new(format!("{:.0} fps", self.fps)).text_color(rgb(C_MUTED)))
            .child(Label::new(format!("потеряно {}", self.lost_frames)).text_color(rgb(C_MUTED)));

        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .bg(rgb(C_PANEL_HI))
            .border_b_1()
            .border_color(rgb(C_BORDER))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(9.)).h(px(9.)).rounded(px(2.)).bg(rgb(C_ACCENT)))
                    .child(Label::new("РЕЖИМ").text_color(rgb(C_MUTED)))
                    .child(Label::new(self.tab_title()).text_color(rgb(C_TEXT))),
            )
            .child(div().w(px(1.)).h(px(20.)).bg(rgb(C_BORDER)))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(self.tab_button(cx, "∿", "Осциллограф", Tab::Scope))
                    .child(self.tab_button(cx, "≡", "Спектр", Tab::Spectrum))
                    .child(self.tab_button(cx, "Ω", "LCR", Tab::Lcr))
                    .child(self.tab_button(cx, "↗", "Переходный", Tab::Transient)),
            )
            .child(div().flex_1())
            .child(status)
            .into_any_element()
    }

    /// Левая панель каналов: маркер цвета, вкл/выкл канала.
    fn render_channel_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = v_flex()
            .w(px(200.))
            .h_full()
            .bg(rgb(C_PANEL))
            .border_r_1()
            .border_color(rgb(C_BORDER))
            .p_1()
            .gap(px(2.))
            .child(self.section_header(cx, "sec-channels", "КАНАЛЫ", PanelSection::Channels));

        for ch in 0..10usize {
            if self.open_section != Some(PanelSection::Channels) {
                break;
            }
            let enabled = self.ch_enabled[ch];
            let vpp = if enabled { self.channel_vpp(ch) } else { 0.0 };
            let row = h_flex()
                .w_full()
                .px_1()
                .gap_1()
                .items_center()
                .rounded(px(3.))
                .bg(if ch == self.selected_ch {
                    rgb(C_SEL)
                } else if enabled {
                    rgb(C_PANEL_HI)
                } else {
                    rgb(C_PANEL)
                })
                .child(
                    div()
                        .id(format!("ch-mark-{ch}"))
                        .w(px(7.))
                        .h(px(13.))
                        .rounded(px(2.))
                        .bg(if enabled { channel_color(ch) } else { rgb(C_OFF).into() })
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_channel(ch, cx))),
                )
                .child(
                    div()
                        .id(format!("ch-sel-{ch}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected_ch = ch;
                            cx.notify();
                        }))
                        .child(
                            Label::new(format!("CH{ch}"))
                                .text_color(if enabled { rgb(C_TEXT) } else { rgb(C_MUTED) }),
                        ),
                )
                .child(div().flex_1())
                .child(Label::new(format!("{vpp:.2}В")).text_color(rgb(C_MUTED)))
                .child(
                    Button::new(format!("ch-toggle-{ch}"))
                        .label(if enabled { "●" } else { "○" })
                        .compact()
                        .selected(enabled)
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_channel(ch, cx))),
                );
            panel = panel.child(row);
        }

        panel = panel.child(self.section_header(cx, "sec-timebase", "РАЗВЁРТКА", PanelSection::Timebase));
        if self.open_section == Some(PanelSection::Timebase) {
            let t = self.time_div_ms;
            let t_str = if t < 1.0 {
                format!("{:.0} мкс", t * 1000.0)
            } else {
                format!("{t:.1} мс")
            };
            let v = self.v_per_div;
            let v_str = if v < 1.0 {
                format!("{:.0} мВ", v * 1000.0)
            } else {
                format!("{v:.0} В")
            };

            panel = panel
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(Label::new("Время").text_color(rgb(C_MUTED)))
                    .child(div().flex_1())
                    .child(
                        Button::new("td-minus")
                            .label("−")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_time_div(-1);
                                cx.notify();
                            })),
                    )
                    .child(Label::new(t_str).text_color(rgb(C_TEXT)))
                    .child(
                        Button::new("td-plus")
                            .label("+")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_time_div(1);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(Label::new("Вольт").text_color(rgb(C_MUTED)))
                    .child(div().flex_1())
                    .child(
                        Button::new("vd-minus")
                            .label("−")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_v_div(-1);
                                cx.notify();
                            })),
                    )
                    .child(Label::new(v_str).text_color(rgb(C_TEXT)))
                    .child(
                        Button::new("vd-plus")
                            .label("+")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_v_div(1);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                Button::new("trigger-toggle")
                    .label(if self.trigger_on { "Триггер: вкл" } else { "Триггер: выкл" })
                    .compact()
                    .selected(self.trigger_on)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.trigger_on = !this.trigger_on;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("auto-set")
                    .label("АВТО")
                    .tooltip("Подстроить развёртку под сигнал")
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.auto_set();
                        cx.notify();
                    })),
            );
        }

        panel = panel.child(self.section_header(cx, "sec-sync", "СИНХРОНИЗАЦИЯ", PanelSection::Sync));
        if self.open_section == Some(PanelSection::Sync) {
            panel = panel
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(Label::new("Част. выб.").text_color(rgb(C_MUTED)))
                    .child(div().flex_1())
                    .child(
                        Button::new("rate-minus")
                            .label("−")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_rate(-1);
                                cx.notify();
                            })),
                    )
                    .child(Label::new(format!("ур. {}", self.rate_level)).text_color(rgb(C_TEXT)))
                    .child(
                        Button::new("rate-plus")
                            .label("+")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_rate(1);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(Label::new(format!("Смещ. CH{}", self.selected_ch)).text_color(rgb(C_MUTED)))
                    .child(div().flex_1())
                    .child(
                        Button::new("off-minus")
                            .label("−")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_offset(-1);
                                cx.notify();
                            })),
                    )
                    .child(Label::new(format!("{:+.1} В", self.ch_offset_v[self.selected_ch])).text_color(rgb(C_TEXT)))
                    .child(
                        Button::new("off-plus")
                            .label("+")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_offset(1);
                                cx.notify();
                            })),
                    ),
            );
        }

        panel = panel.child(self.section_header(cx, "sec-cursors", "КУРСОРЫ", PanelSection::Cursors));
        if self.open_section == Some(PanelSection::Cursors) {
            panel = panel
            .child(
                Button::new("cursors-toggle")
                    .label(if self.cursors_on { "Курсоры: вкл" } else { "Курсоры: выкл" })
                    .compact()
                    .selected(self.cursors_on)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cursors_on = !this.cursors_on;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(Label::new("A").text_color(rgb(C_TEXT)))
                    .child(div().flex_1())
                    .child(
                        Button::new("ca-minus")
                            .label("−")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cursor_a = (this.cursor_a - 0.02).clamp(0.0, 1.0);
                                cx.notify();
                            })),
                    )
                    .child(Label::new(format!("{:.0}%", self.cursor_a * 100.0)).text_color(rgb(C_TEXT)))
                    .child(
                        Button::new("ca-plus")
                            .label("+")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cursor_a = (this.cursor_a + 0.02).clamp(0.0, 1.0);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(Label::new("B").text_color(rgb(C_TEXT)))
                    .child(div().flex_1())
                    .child(
                        Button::new("cb-minus")
                            .label("−")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cursor_b = (this.cursor_b - 0.02).clamp(0.0, 1.0);
                                cx.notify();
                            })),
                    )
                    .child(Label::new(format!("{:.0}%", self.cursor_b * 100.0)).text_color(rgb(C_TEXT)))
                    .child(
                        Button::new("cb-plus")
                            .label("+")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cursor_b = (this.cursor_b + 0.02).clamp(0.0, 1.0);
                                cx.notify();
                            })),
                    ),
            );
        }

        panel.into_any_element()
    }

    /// Байт конфигурации генератора для команды 0xC0 (см. firmware/src/main.rs).
    fn gen_cfg(&self) -> u8 {
        (self.gen_en as u8)
            | ((self.gen_shape & 0x3) << 1)
            | (((self.gen_amp.max(1).min(8) - 1) & 0x7) << 3)
    }

    fn send_gen(&self) {
        if let Some(t) = &self.transport {
            t.set_gen(self.gen_cfg());
        }
    }

    /// Заголовок сворачиваемой секции левой панели (аккордеон: открыта максимум одна).
    fn section_header(
        &self,
        cx: &mut Context<Self>,
        id: &'static str,
        title: &'static str,
        section: PanelSection,
    ) -> AnyElement {
        let open = self.open_section == Some(section);
        h_flex()
            .id(id)
            .w_full()
            .px_1()
            .py(px(1.))
            .gap(px(4.))
            .items_center()
            .cursor_pointer()
            .child(Label::new(if open { "▾" } else { "▸" }).text_color(rgb(C_MUTED)))
            .child(Label::new(title).text_color(if open { rgb(C_TEXT) } else { rgb(C_MUTED) }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_section = if this.open_section == Some(section) {
                    None
                } else {
                    Some(section)
                };
                cx.notify();
            }))
            .into_any_element()
    }

    /// Таблица измерений выбранного канала — отдельная колонка справа от графиков.
    fn render_measurements(&self) -> AnyElement {
        let rows: Vec<(String, String)> = match self.selected_metrics() {
            Some(m) => {
                let f = if m.freq_hz >= 1000.0 {
                    format!("{:.2} кГц", m.freq_hz / 1000.0)
                } else {
                    format!("{:.1} Гц", m.freq_hz)
                };
                let per = if m.period_s < 1e-3 {
                    format!("{:.0} мкс", m.period_s * 1e6)
                } else {
                    format!("{:.2} мс", m.period_s * 1e3)
                };
                vec![
                    ("Vpp".to_string(), format!("{:.3} В", m.vpp)),
                    ("Vrms".to_string(), format!("{:.3} В", m.vrms)),
                    ("Vср".to_string(), format!("{:.3} В", m.vavg)),
                    ("Vмакс".to_string(), format!("{:.3} В", m.vmax)),
                    ("Vмин".to_string(), format!("{:.3} В", m.vmin)),
                    ("Частота".to_string(), f),
                    ("Период".to_string(), per),
                    ("Скважн.".to_string(), format!("{:.0} %", m.duty * 100.0)),
                ]
            }
            None => vec![("—".to_string(), "нет данных".to_string())],
        };
        let mut col = v_flex()
            .w(px(170.))
            .h_full()
            .bg(rgb(C_PANEL))
            .border_l_1()
            .border_color(rgb(C_BORDER))
            .p_2()
            .gap_1()
            .child(Label::new(format!("ИЗМЕРЕНИЯ CH{}", self.selected_ch)).text_color(rgb(C_TEXT)));
        for (name, val) in rows {
            col = col.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .child(Label::new(name).text_color(rgb(C_MUTED)))
                    .child(div().flex_1())
                    .child(Label::new(val).text_color(rgb(C_TEXT))),
            );
        }
        col.into_any_element()
    }

    fn render_gen_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let shapes = [("○", "Выкл", 0u8), ("∿", "Синус", 1u8), ("△", "Треугольник", 2u8), ("⊓", "Меандр", 3u8)];
        let mut row = h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .bg(rgb(C_PANEL))
            .border_t_1()
            .border_color(rgb(C_BORDER))
            .child(Label::new("ГЕНЕРАТОР").text_color(if self.gen_en { rgb(C_TEXT) } else { rgb(C_OFF) }))
            .child(div().w(px(1.)).h(px(18.)).bg(rgb(C_BORDER)));
        for (glyph, title, shape) in shapes {
            let active = if shape == 0 {
                !self.gen_en
            } else {
                self.gen_en && self.gen_shape == shape
            };
            row = row.child(
                div()
                    .id(title)
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .bg(if active { rgb(C_ACCENT) } else { rgb(C_PANEL_HI) })
                    .border_1()
                    .border_color(if active { rgb(C_ACCENT) } else { rgb(C_BORDER) })
                    .child(
                        Label::new(glyph).text_color(if active { rgb(0xffffff) } else { rgb(C_TEXT) }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.gen_shape = shape.max(1);
                        this.gen_en = shape != 0;
                        this.send_gen();
                        cx.notify();
                    })),
            );
        }
        row = row.child(div().w(px(12.)));
        row = row.child(
            Label::new("АМПЛИТУДА").text_color(if self.gen_en { rgb(C_MUTED) } else { rgb(C_OFF) }),
        );
        for amp in 1u8..=8 {
            let active = self.gen_en && self.gen_amp == amp;
            row = row.child(
                div()
                    .id(format!("gen-amp-{amp}"))
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .bg(if active { rgb(C_ACCENT) } else { rgb(C_PANEL_HI) })
                    .border_1()
                    .border_color(if active { rgb(C_ACCENT) } else { rgb(C_BORDER) })
                    .child(
                        Label::new(format!("{amp}"))
                            .text_color(if active { rgb(0xffffff) } else { rgb(C_TEXT) }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.gen_amp = amp;
                        this.send_gen();
                        cx.notify();
                    })),
            );
        }
        row.into_any_element()
    }
}

static RENDER_COUNT: AtomicU64 = AtomicU64::new(0);

impl Render for LabApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let n = RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 3 || n % 300 == 0 {
            eprintln!(
                "[labbench] render #{n} tab={:?} frame_count={}",
                self.tab, self.frame_count
            );
        }
        let toolbar = self.render_toolbar(cx);
        let panel = self.render_channel_panel(cx);
        let content: AnyElement = match self.tab {
            Tab::Scope => self.render_scope(cx),
            Tab::Spectrum => self.render_spectrum(cx),
            Tab::Lcr => self.render_lcr(cx),
            Tab::Transient => self.render_transient(cx),
        };
        let genbar = self.render_gen_bar(cx);
        // Строгий приборный layout: тулбар · (панель | график | измерения) · генератор,
        // средняя секция растягивается, генератор прижат к нижней границе окна.
        v_flex()
            .size_full()
            .bg(rgb(C_BG))
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .items_stretch()
                    .child(panel)
                    .child(
                        h_flex()
                            .flex_1()
                            .h_full()
                            .items_stretch()
                            .child(div().flex_1().h_full().p_3().child(content))
                            .child(self.render_measurements()),
                    ),
            )
            .child(genbar)
    }
}

fn noise(k: usize) -> f32 {
    // Детерминированный «шум» для симулятора — псевдослучайное число из LCG-хэша, ±0.02.
    let mut x = k.wrapping_mul(2654435761);
    x ^= x >> 16;
    x = x.wrapping_mul(2246822519);
    x ^= x >> 15;
    (((x & 0xFFFF) as f32 / 32768.0) - 1.0) * 0.02
}

fn main() {
    let cli = Cli::parse();
    eprintln!(
        "[labbench] start: port={:?} baud={} simulate={} DISPLAY={:?}",
        cli.port,
        cli.baud,
        cli.simulate,
        std::env::var("DISPLAY").ok()
    );

    gpui_kit::application().run(move |cx| {
        eprintln!("[labbench] app init…");
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        eprintln!("[labbench] opening window…");
        // На X11-бэкенде вариант Maximized лишь поставляет restore-size
        // (`window_bounds.get_bounds()`), поэтому передаём реальные границы
        // основного дисплея — окно открывается «во весь экран».
        let max_bounds = Bounds::maximized(None, cx);
        eprintln!("[labbench] display bounds: {max_bounds:?}");
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Maximized(max_bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("STM32F3Discovery oscilloscope v 0.2.4".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        match gpui_kit::open_window(options, cx, |window, cx| {
            // X11-бэкенд игнорирует вариант Maximized в WindowOptions, поэтому
            // просим WM развернуть окно явно (после создания).
            window.zoom_window();
            cx.new(|cx| LabApp::new(cli.port.clone(), cli.baud, cli.simulate, cx))
        }) {
            Ok((handle, _view)) => eprintln!("[labbench] window opened: {handle:?}"),
            Err(e) => eprintln!("[labbench] open_window FAILED: {e:?}"),
        }
    });
}

// clap CLI
use clap::Parser;
#[derive(Parser, Debug)]
#[command(name = "labbench", about = "gpui-kit oscilloscope/spectrum/LCR/Z meter")]
struct Cli {
    #[arg(long)]
    port: Option<String>,
    #[arg(long, default_value_t = 115200)]
    baud: u32,
    #[arg(long)]
    simulate: bool,
}
