//! LabBench — gpui-kit-фронтенд измерительного комплекса (gpui-kit 0.7.1)
//! Вкладки: Oscilloscope · Spectrum · LCR · Transient (Z-метр).

mod canvas;
mod dsp;
mod transport;

use std::rc::Rc;
use std::time::{Duration, Instant};

// --- Явные импорты для gpui-kit 0.7.1 ---
use gpui_kit::base::Selectable;
use gpui_kit::component::{
    button::Button, chart::LineChart, h_flex, label::Label, separator::Separator, v_flex,
};
use gpui_kit::*;

use serde::Deserialize;

use canvas::TraceCanvas;
use transport::Transport;

/// Максимум точек на канал в кольцевом буфере осциллографа.
const RING_LEN: usize = 4096;
/// Частота дискретизации симулятора.
const SIM_FS: f32 = 72_000.0;
/// Период опроса транспорта/симулятора в UI-потоке.
const PUMP_INTERVAL: Duration = Duration::from_millis(33);

#[derive(Clone, Copy, PartialEq, Deserialize)]
enum Tab {
    Scope,
    Spectrum,
    Lcr,
    Transient,
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
        let y0 = self.channel_slice(0);
        if y0.len() >= 256 {
            let n = y0.len().next_power_of_two().min(y0.len());
            let win = dsp::hann(n);
            let (fr, db) = dsp::amplitude_db(&y0[..n], fs, &win);
            self.spec_freqs = Rc::new(fr);
            self.spec_dbs = Rc::new(db);
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
        let mut out = Vec::new();
        for ch in 0..10 {
            if self.ch_enabled[ch] {
                let raw = self.channel_slice(ch);
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

    fn render_scope(&self, _cx: &Context<Self>) -> AnyElement {
        let chans = self.visible_channels();
        let fs = self.fs_hz;

        let mut header = h_flex()
            .gap_4()
            .items_center()
            .child(Label::new(format!("{:.1} ms/div", self.time_div_ms)))
            .child(Label::new(format!("fs = {:.0} Гц", fs)))
            .child(Label::new(format!("{:.0} fps", self.fps)));
        if self.lost_frames > 0 {
            header = header.child(Label::new(format!("lost {}", self.lost_frames)));
        }
        if self.trigger_on {
            header = header.child(Label::new("trig"));
        }

        h_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(header)
                    .child(
                        div()
                            .id("scope-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(TraceCanvas::new(chans).trigger_level(if self.trigger_on {
                                Some(0.0)
                            } else {
                                None
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_spectrum(&self, _cx: &Context<Self>) -> AnyElement {
        let points: Vec<(SharedString, f32)> = self
            .spec_freqs
            .iter()
            .zip(self.spec_dbs.iter())
            .map(|(&f, &db)| (format!("{f:.0}").into(), db))
            .collect();

        h_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_4()
                            .items_center()
                            .child(Label::new(format!("fs = {:.0} Гц", self.fs_hz)))
                            .child(Label::new(format!("{:.0} fps", self.fps))),
                    )
                    .child(
                        div()
                            .id("spectrum-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                LineChart::new(points)
                                    .id("spectrum-chart")
                                    .x(|(f, _)| f.clone())
                                    .y(|(_, db)| *db)
                                    .natural()
                                    .x_axis(true)
                                    .y_axis(true)
                                    .grid(true),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_lcr(&self, _cx: &Context<Self>) -> AnyElement {
        let body = match self.lcr_result {
            Some((z, val, unit, freq)) => v_flex()
                .items_start()
                .gap_4()
                .child(Label::new(format!("Частота теста: {freq:.0} Гц")))
                .child(Separator::horizontal())
                .child(
                    h_flex()
                        .gap_4()
                        .items_center()
                        .child(Label::new("Z:"))
                        .child(Label::new(format!("{val:.2} {unit}"))),
                )
                .child(
                    h_flex()
                        .gap_4()
                        .items_center()
                        .child(Label::new("R+jX:"))
                        .child(Label::new(format!("{:.3} + j{:.3} Ом", z.re, z.im))),
                )
                .child(Label::new(&self.status_line)),
            None => v_flex()
                .items_center()
                .justify_center()
                .child(Label::new("Ожидание данных LCR…")),
        };

        h_flex().size_full().child(body).into_any_element()
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

        let points: Vec<(SharedString, f32)> = data
            .iter()
            .enumerate()
            .map(|(i, &v)| (format!("{i}").into(), v))
            .collect();

        h_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_4()
                            .items_center()
                            .child(Label::new(format!("Rise time 10–90%: {tr:.3} мкс")))
                            .child(Label::new(format!("{:.0} fps", self.fps))),
                    )
                    .child(
                        div()
                            .id("transient-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                LineChart::new(points)
                                    .id("transient-chart")
                                    .x(|(x, _)| x.clone())
                                    .y(|(_, v)| *v)
                                    .natural(),
                            ),
                    )
                    .child(Label::new(&self.status_line)),
            )
            .into_any_element()
    }

    fn tab_button(&self, cx: &mut Context<Self>, label: &'static str, tab: Tab) -> Button {
        Button::new(label)
            .label(label)
            .selected(self.tab == tab)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tab = tab;
                cx.notify();
            }))
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .p_2()
            .gap_1()
            .items_center()
            .child(self.tab_button(cx, "Oscilloscope", Tab::Scope))
            .child(self.tab_button(cx, "Spectrum", Tab::Spectrum))
            .child(self.tab_button(cx, "LCR", Tab::Lcr))
            .child(self.tab_button(cx, "Transient", Tab::Transient))
            .into_any_element()
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

    fn render_gen_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let shapes = [("Выкл", 0u8), ("Синус", 1u8), ("Треугольник", 2u8), ("Меандр", 3u8)];
        let mut row = h_flex()
            .p_2()
            .gap_1()
            .items_center()
            .child(Label::new("Генератор:"));
        for (name, shape) in shapes {
            let selected = if shape == 0 {
                !self.gen_en
            } else {
                self.gen_en && self.gen_shape == shape
            };
            let btn = Button::new(name)
                .label(name)
                .compact()
                .selected(selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.gen_shape = shape.max(1);
                    this.gen_en = shape != 0;
                    this.send_gen();
                    cx.notify();
                }));
            row = row.child(btn);
        }
        row.child(
            Button::new("gen-amp")
                .label(format!("Амплитуда: {}/8", self.gen_amp))
                .compact()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.gen_amp = if this.gen_amp >= 8 { 1 } else { this.gen_amp + 1 };
                    this.send_gen();
                    cx.notify();
                })),
        )
        .into_any_element()
    }
}

impl Render for LabApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_bar = self.render_tab_bar(cx);
        let gen_bar = self.render_gen_bar(cx);
        let content: AnyElement = match self.tab {
            Tab::Scope => self.render_scope(cx),
            Tab::Spectrum => self.render_spectrum(cx),
            Tab::Lcr => self.render_lcr(cx),
            Tab::Transient => self.render_transient(cx),
        };
        v_flex().size_full().child(tab_bar).child(gen_bar).child(content)
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

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |_window, cx| {
            cx.new(|cx| LabApp::new(cli.port.clone(), cli.baud, cli.simulate, cx))
        })
        .expect("failed to open window");
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
