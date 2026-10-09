//! LabBench — gpui-kit-фронтенд измерительного комплекса (gpui-kit 0.7.1)
//! Вкладки: Oscilloscope · Spectrum · LCR · Transient (Z-метр).

mod canvas;
mod dsp;
mod transport;

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

// --- Явные импорты для gpui-kit 0.7.1 ---
use gpui_kit::component::{label::Label, badge::Badge, checkbox::Checkbox, button::Button, chart::LineChart, Divider};
use gpui_kit::Application;
use gpui_kit::prelude::*;

// Для flex/div/h_flex/v_flex — обычно они в корне или в components,
// но в 0.7 чаще всего доступны через prelude или напрямую.
// Если компилятор ругается на h_flex/div — попробуй добавить:
// use gpui_kit::elements::{h_flex, v_flex, div};
// или проверь, экспортируются ли они в твоем билде из components.
use gpui_kit::component::{h_flex, v_flex, div};

use serde::Deserialize;

use canvas::TraceCanvas;
use transport::Transport;

// #[derive(clap::Parser, Debug, Clone)]
// #[command(name = "labbench")]
// struct Cli {
//     #[arg(short, long)]
//     port: Option<String>,
//     #[arg(short, long, default_value = "115200")]
//     baud: u32,
//     #[arg(long, default_value = "false")]
//     simulate: bool,
// }

/// Максимум точек на канал в кольцевом буфере осциллографа.
const RING_LEN: usize = 4096;
/// Частота дискретизации симулятора.
const SIM_FS: f32 = 72_000.0;

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
    lcr_result: Option<(dsp::Impedance, f64, &'static str, f32)>,
    transient_result: Option<(f32, Rc<Vec<f32>>)>,

    status_line: String,
}

impl LabApp {
    pub fn new(
        port: Option<String>,
        baud: u32,
        simulate: bool,
        window: Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let transport = if simulate && port.is_none() {
            None
        } else {
            Some(Transport::spawn(port, baud))
        };

        Self {
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
            lcr_result: None,
            transient_result: None,
            status_line: "ожидание данных…".to_string(),
        }
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
                        let _ = t_again.get_mask();
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
                            self.status_line =
                                format!("fs ≈ {:.0} Гц (rate={level})", sample_rate(level));
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
                        Ack::Dfu => {
                            self.status_line = "переход в DFU…".to_string();
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

                let k = ((self.sim_phase * SIM_FS as f64) as usize) % 8192;
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
        let y0 = self.channel_slice(0);
        if y0.len() >= 256 {
            let n = y0.len().next_power_of_two().min(y0.len());
            let win = dsp::hann(n);
            let (fr, db) = dsp::amplitude_db(&y0[..n], sample_rate(self.rate_level), &win);
            self.spec_freqs = Rc::new(fr);
            self.spec_dbs = Rc::new(db);
        }

        let y1 = self.channel_slice(1);
        let current_y0 = self.channel_slice(0);
        let n = current_y0.len().min(y1.len()).min(4096);
        if n >= 256 {
            let f_test = 5_000.0f32;
            let z = dsp::measure_z(&current_y0[..n], &y1[..n], f_test, sample_rate(self.rate_level), 100.0);
            let (val, unit) = dsp::lcr(z, f_test);
            self.lcr_result = Some((z, val, unit, f_test));
        }

        let y2 = self.channel_slice(2);
        if y2.len() >= 256 {
            let tr = dsp::rise_time_10_90(&y2, sample_rate(self.rate_level));
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

    fn render_scope(&self, _cx: &Context<Self>) -> impl IntoElement {
        let chans = self.visible_channels();
        let fs = sample_rate(self.rate_level);

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
                            .child(Label::new(format!("{:.1} ms/div", self.time_div_ms)))
                            .child(Label::new(format!("fs = {:.0} Гц", fs)))
                            .child(Label::new(format!("{:.0} fps", self.fps)))
                            .when(self.lost_frames > 0, |e| {
                                e.child(Badge::new(format!("lost {}", self.lost_frames)).warning())
                            })
                            .when(self.trigger_on, |e| {
                                e.child(Badge::new("trig").success())
                            }),
                    )
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
                            }))
                            .into_element(),
                    ),
            )
            .into_element()
    }
    fn render_spectrum(&self, _cx: &Context<Self>) -> impl IntoElement {
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
                            .child(Label::new(format!("fs = {:.0} Гц", sample_rate(self.rate_level))))
                            .child(Label::new(format!("{:.0} fps", self.fps))),
                    )
                    .child(
                        div()
                            .id("spectrum-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                LineChart::new()
                                    .x_values(self.spec_freqs.clone())
                                    .y_values(self.spec_dbs.clone())
                                    .label("Спектр")
                                    .into_element(),
                            )
                            .into_element(),
                    ),
            )
            .into_element()
    }

    fn render_lcr(&self, _cx: &Context<Self>) -> impl IntoElement {
        let (z, val, unit, freq) = match self.lcr_result {
            Some(r) => r,
            None => {
                // Заглушка, если данных ещё нет
                return h_flex()
                    .size_full()
                    .child(
                        v_flex()
                            .items_center()
                            .justify_center()
                            .child(Label::new("Ожидание данных LCR…"))
                            .into_element(),
                    )
                    .into_element();
            }
        };

        h_flex()
            .size_full()
            .child(
                v_flex()
                    .items_start()
                    .gap_4()
                    .child(Label::new(format!("Частота теста: {freq:.0} Гц")))
                    .child(Divider::new())
                    // Вместо KeyValue (если его нет) — пара Label для наглядности
                    .child(
                        h_flex()
                            .gap_4()
                            .items_center()
                            .child(Label::new("Z:"))
                            .child(Label::new(format!("{val:.2} {unit}")))
                            .into_element(),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .items_center()
                            .child(Label::new("R+jX:"))
                            .child(Label::new(format!("{}", z)))
                            .into_element(),
                    )
                    .child(Label::new(&self.status_line))
                    .into_element(),
            )
            .into_element()
    }

    fn render_transient(&self, _cx: &Context<Self>) -> impl IntoElement {
        let (tr, data) = match self.transient_result {
            Some(r) => r,
            None => {
                return h_flex()
                    .size_full()
                    .child(
                        v_flex()
                            .items_center()
                            .justify_center()
                            .child(Label::new("Ожидание переходного процесса…"))
                            .into_element(),
                    )
                    .into_element();
            }
        };

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
                            .child(Label::new(format!("{:.0} fps", self.fps)))
                            .into_element(),
                    )
                    .child(
                        div()
                            .id("transient-canvas")
                            .flex_1()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                // Тут можно подставить свой компонент для отрисовки переходного процесса
                                // или использовать LineChart по аналогии со спектром
                                Label::new("График переходного процесса (заглушка)")
                            )
                            .into_element(),
                    )
                    .child(Label::new(&self.status_line))
                    .into_element(),
            )
            .into_element()
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .p_2()
            .gap_1()
            .items_center()
            .child(
                Button::new(cx, "Oscilloscope", {
                    move |_, cx| {
                        cx.set_global(RootView(/* тут логика переключения */));
                        // В реальном коде здесь нужно корректно переключить tab
                        // Для простоты пока просто меняем поле и уведомляем
                        if let Some(root) = cx.try_global::<RootView>() {
                            root.0.update(cx, |v, cx| v.on_switch_tab(&Tab::Scope, cx));
                        }
                    }
                })
                .selected(self.tab == Tab::Scope)
                .into_element(),
            )
            .child(
                Button::new(cx, "Spectrum", {
                    move |_, cx| {
                        if let Some(root) = cx.try_global::<RootView>() {
                            root.0.update(cx, |v, cx| v.on_switch_tab(&Tab::Spectrum, cx));
                        }
                    }
                })
                .selected(self.tab == Tab::Spectrum)
                .into_element(),
            )
            .child(
                Button::new(cx, "LCR", {
                    move |_, cx| {
                        if let Some(root) = cx.try_global::<RootView>() {
                            root.0.update(cx, |v, cx| v.on_switch_tab(&Tab::Lcr, cx));
                        }
                    }
                })
                .selected(self.tab == Tab::Lcr)
                .into_element(),
            )
            .child(
                Button::new(cx, "Transient", {
                    move |_, cx| {
                        if let Some(root) = cx.try_global::<RootView>() {
                            root.0.update(cx, |v, cx| v.on_switch_tab(&Tab::Transient, cx));
                        }
                    }
                })
                .selected(self.tab == Tab::Transient)
                .into_element(),
            )
            .into_element()
    }

    pub fn render(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(self.render_tab_bar(cx).into_element())
            .child(
                match self.tab {
                    Tab::Scope => self.render_scope(cx).into_element(),
                    Tab::Spectrum => self.render_spectrum(cx).into_element(),
                    Tab::Lcr => self.render_lcr(cx).into_element(),
                    Tab::Transient => self.render_transient(cx).into_element(),
                }
            )
            .into_element()
    }
}

// --- Глобальный тип для хранения корневого вида (если у тебя уже есть — используй свой) ---
// #[derive(Clone)]
// struct RootView(Arc<LabApp>);

impl gpui_kit::Global for RootView {}

fn sample_rate(level: u8) -> f32 {
    // Примерная логика: подставь свою реальную формулу
    match level {
        0 => 1_000.0,
        1 => 4_000.0,
        2 => 8_000.0,
        3 => 16_000.0,
        4 => 32_000.0,
        5 => 64_000.0,
        6 => 128_000.0,
        _ => 16_000.0,
    }
}

fn noise(_k: usize) -> f32 {
    // Заглушка для шума — в продакшене используй нормальный генератор
    0.0
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    Application::with_platform()
        .run(|mut cx| {
            cx.set_global(RootView(Default::default()));

            cx.open_window(WindowOptions::default(), |window, mut cx| {
                let app = cx.new(|mut cx| LabApp::new(cli.port.clone(), cli.baud, cli.simulate, window, &mut cx));
                cx.set_global(RootView(app.clone()));
                app
            });
        });
}


/// Обёртка-глобалка: даёт on_action-обработчикам доступ к корневому view.
struct RootView(Entity<LabApp>);
impl gpui_kit::Global for RootView {}

// clap CLI
use clap::Parser;
#[derive(Parser, Debug)]
#[command(name = "labbench", about = "gpui-kit oscilloscope/spectrum/LCR/Z meter")]
struct Cli {
    /// Порт USB-CDC (по умолчанию автодетект ttyACM/ttyUSB/COM)
    #[arg(long)]
    port: Option<String>,
    #[arg(long, default_value_t = 115200)]
    baud: u32,
    /// Работать на встроенном симуляторе сигналов (без железа)
    #[arg(long)]
    simulate: bool,
}
