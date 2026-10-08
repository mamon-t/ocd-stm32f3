//! LabBench — gpui-kit-фронтенд измерительного комплекса.
//!
//! Вкладки: Oscilloscope · Spectrum · LCR · Transient (Z-метр).
//! Источник данных два: живое устройство по USB-CDC (`--port`) или
//! встроенный симулятор сигналов (`--simulate`) — второй позволяет верифицировать
//! UI и алгоритмы без STM32 и в headless-среде.

mod canvas;
mod dsp;
mod transport;

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::*;
use gpui_kit::*;

use canvas::TraceCanvas;
use transport::Transport;

/// Максимум точек на канал в кольцевом буфере осциллографа (как MAX_SAMPLES в Python).
const RING_LEN: usize = 4096;
/// Частота дискретизации симулятора / «известная» fs устройства для расчётов.
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

    /// Кольцевые буферы 10 каналов (сырые u16 → нормализуем при рендере)
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

    // --- состояние устройства (протокол v2) ---
    last_seq: Option<u8>,
    lost_frames: u64,

    // --- результаты DSP (пересчитываются при приходе кадров) ---
    spec_freqs: Rc<Vec<f32>>,
    spec_dbs: Rc<Vec<f32>>,
    /// маска включённых каналов устройства (из ACK_MASK) — по ней сопоставляем
    /// слоты кадра v2 с номерами каналов
    dev_mask: u16,
    lcr_result: Option<(dsp::Impedance, f64, &'static str, f32)>, // Z, значение, единица, f
    transient_result: Option<(f32, Rc<Vec<f32>>)>,                // t_r, огибающая step
    status_line: SharedString,
}

impl LabApp {
    pub fn new(
        port: Option<String>,
        baud: u32,
        simulate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let transport = if simulate && port.is_none() {
            None
        } else {
            Some(Transport::spawn(port, baud))
        };

        let this = Self {
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
            dev_mask: 0x03, // по умолчанию прошивка отдаёт CH0+CH1 (подтверждается ACK_MASK)
            lcr_result: None,
            transient_result: None,
            status_line: "ожидание данных…".into(),
        };

        // Главный цикл данных: раз в ~16 мс забираем всё, что натёк из порта
        // (или генерируем симуляцию), пересчитываем DSP и notify().
        let ticker = cx.spawn_in(window, async move |slf, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let _ = slf.update_in(cx, |app, _w, cx| {
                    app.pump(cx);
                });
            }
        });
        let _ = ticker; // живёт до конца работы приложения
        this
    }

    // ========================================================================
    // Поток данных
    // ========================================================================

    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut got_any = false;

        if let Some(t) = &self.transport {
            while let Ok(ev) = t.receiver().try_recv() {
                match ev {
                    transport::Event::Connected(name) => {
                        self.status_line = format!("подключено: {name}").into();
                        self.last_seq = None;
                        // синхронизируем UI с реальным состоянием устройства
                        t.get_mask();
                        cx.notify();
                    }
                    transport::Event::Status(s) => {
                        self.status_line = s.into();
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
                            self.status_line = "ADC/DMA overrun на устройстве".into();
                        }
                        self.push_frame(&channels);
                        got_any = true;
                    }
                    transport::Event::Ack(ack) => {
                        use transport::Ack;
                        match ack {
                            Ack::Rate(level) => {
                                self.rate_level = level;
                                self.status_line =
                                    format!("fs ≈ {:.0} Гц (rate={level})", sample_rate(level)).into();
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
                                self.status_line = "переход в DFU…".into();
                            }
                        }
                        cx.notify();
                    }
                }
            }
        }

        if !got_any && self.simulate {
            // Симулятор: 1 кГц + 5 кГц на CH0, шум на CH1, шаг с экспонентой —
            // на транзиентную вкладку. Кадр = 128 отсчётов, ~каждый тик.
            let n = 128;
            let mut buf = Vec::with_capacity(n);
            for i in 0..n {
                let t = (self.sim_phase + i as f64 / SIM_FS as f64) as f32;
                let v0 = 0.7 * ((std::f64::consts::TAU * 1000.0 * t as f64).sin() as f32)
                    + 0.25 * ((std::f64::consts::TAU * 5000.0 * t as f64).sin() as f32);
                let v1 = 0.4 * ((std::f64::consts::TAU * 300.0 * t as f64).cos() as f32);
                // «step-ответ» RC: сбрасываемся каждые 8192 отсчётов
                let k = ((self.sim_phase * SIM_FS as f64) as usize) % 8192;
                let v_step = 1.0 - (-k as f32 / (SIM_FS * 1e-3)).exp();
                buf.push(v0.max(-1.0).min(1.0));
                self.rings[0][k % RING_LEN] = v0.max(-1.0).min(1.0);
                self.rings[1][k % RING_LEN] = (v1 + noise(k)).max(-1.0).min(1.0);
                self.rings[2][k % RING_LEN] = v_step * 2.0 - 1.0;
            }
            self.sim_phase += n as f64 / SIM_FS as f64;
            got_any = true;
            let _ = buf;
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
        // Кадр v2 = один скан АЦП: слот i соответствует i-му включённому каналу
        // по маске устройства (dev_mask). Пишем каждый отсчёт в своё кольцо.
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

    /// Линейная копия кольца (от newest назад) для трассы.
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
        // --- спектр CH0 ---
        let y0 = self.channel_slice(0);
        if y0.len() >= 256 {
            let n = y0.len().next_power_of_two().min(y0.len());
            let win = dsp::hann(n);
            let (fr, db) = dsp::amplitude_db(&y0[..n], sample_rate(self.rate_level), &win);
            self.spec_freqs = Rc::new(fr);
            self.spec_dbs = Rc::new(db);
        }

        // --- LCR: CH0 = V(dut), CH1 = V(sense), тестовая частота 5 кГц ---
        let y1 = self.channel_slice(1);
        let n = y0.len().min(y1.len()).min(4096);
        if n >= 256 {
            let f_test = 5_000.0f32;
            let z = dsp::measure_z(&y0[..n], &y1[..n], f_test, sample_rate(self.rate_level), 100.0);
            let (val, unit) = dsp::lcr(z, f_test);
            self.lcr_result = Some((z, val, unit, f_test));
        }

        // --- переходные характеристики: фронт на CH2 ---
        let y2 = self.channel_slice(2);
        if y2.len() >= 256 {
            let tr = dsp::rise_time_10_90(&y2, sample_rate(self.rate_level));
            self.transient_result = Some((tr.unwrap_or(0.0), y2.clone()));
        }
    }

    // ========================================================================
    // Рендер
    // ========================================================================

    fn visible_channels(&self) -> Vec<Rc<Vec<f32>>> {
        let mut out = Vec::new();
        for ch in 0..10 {
            if self.ch_enabled[ch] {
                let raw = self.channel_slice(ch);
                // триггер только по первому видимому каналу
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

    fn render_scope(&self, cx: &Context<Self>) -> impl IntoElement {
        let chans = self.visible_channels();
        let fs = sample_rate(self.rate_level);
        let div_s = self.time_div_ms as f32 / 1000.0;
        let per_px = chans[0].len() as f32 / fs; // секунд на всю ширину при 12 div
        let _ = per_px;
        let lost = self.lost_frames;

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
                            .when(lost > 0, |e| {
                                e.child(Badge::new(format!("lost {lost}")).warning())
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
            .child(
                v_flex()
                    .w(px(220.))
                    .gap_2()
                    .p_2()
                    .border_l_1()
                    .child(Label::new("Channels").font_weight(FontWeight::BOLD))
                    .children((0..10).map(|ch| {
                        Checkbox::new(ElementId::from(ch as usize), format!("CH{ch}"))
                            .checked(self.ch_enabled[ch])
                            .on_click(move |_, _, cx| {
                                cx.dispatch_action(ToggleChannel(ch));
                            })
                    }))
                    .child(Label::new("Sample rate").font_weight(FontWeight::BOLD))
                    .children((0u8..8).map(|lvl| {
                        Button::new(ElementId::from(100 + lvl as usize))
                            .label(format!("L{lvl}"))
                            .when(lvl == self.rate_level, |b| b.primary())
                            .when(lvl != self.rate_level, |b| b.ghost())
                            .on_click(move |_, _, cx| {
                                cx.dispatch_action(SetRate(lvl));
                            })
                    }))
                    .child(Label::new("Time/div").font_weight(FontWeight::BOLD))
                    .child(
                        Button::new("tdiv")
                            .label(format!("{:.1} ms", self.time_div_ms))
                            .outline(),
                    ),
            )
    }

    fn render_spectrum(&self, cx: &Context<Self>) -> impl IntoElement {
        // Полосовой график: спектр как min/max-столбики через TraceCanvas?
        // Проще: LineChart из gpui-component для ≤2048 точек.
        let fr = self.spec_freqs.clone();
        let db = self.spec_dbs.clone();
        let peak = dsp::spectral_peak(&fr, &db);

        let points: Vec<(f32, f32)> = fr
            .iter()
            .zip(db.iter())
            .step_by((fr.len() / 1024).max(1))
            .map(|(&f, &d)| (f, d.max(-100.0)))
            .collect();

        v_flex()
            .size_full()
            .gap_2()
            .p_2()
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(Label::new("Analyzer Spectrum · Hann · dBFS").font_weight(FontWeight::BOLD))
                    .child(match peak {
                        Some((f, d)) => Label::new(format!(
                            "peak: {} · {d:.1} dB",
                            human_freq(f)
                        )),
                        None => Label::new("нет сигнала"),
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .child(LineChart::new("spectrum").linear().data(points).color(rgb(0x42d4f4))),
            )
    }

    fn render_lcr(&self, _cx: &Context<Self>) -> impl IntoElement {
        let body = match self.lcr_result {
            Some((z, val, unit, f)) => v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_8()
                        .child(
                            KeyValue::new("Magnitude")
                                .mono_value(format!("{:.1} Ω", z.mag())),
                        )
                        .child(KeyValue::new("Phase").mono_value(format!("{:.1}°", z.phase_deg()))),
                )
                .child(
                    h_flex()
                        .gap_8()
                        .child(KeyValue::new("Re").mono_value(format!("{:.1} Ω", z.re)))
                        .child(KeyValue::new("Im").mono_value(format!("{:.1} Ω", z.im))),
                )
                .child(
                    Divider()
                        .vertical(false),
                )
                .child(
                    h_flex()
                        .gap_8()
                        .items_end()
                        .child(
                            div().text_3xl().font_weight(FontWeight::BOLD).child(format!(
                                "{} {}",
                                human_si(val),
                                unit
                            )),
                        )
                        .child(Label::new(format!("at {}", human_freq(f)))),
                ),
            None => v_flex().child(Label::new("Недостаточно данных (нужны 2 канала)").muted()),
        };

        v_flex()
            .size_full()
            .gap_4()
            .p_4()
            .child(Label::new("LCR Meter · quadrature detector @ 5 kHz").font_weight(FontWeight::BOLD))
            .child(body)
            .child(Label::new(
                "Для реальных измерений: DDS-генератор на таймере STM32 + R_sense; \
                 методика — V/I по двум АЦП-каналам.",
            ))
    }

    fn render_transient(&self, _cx: &Context<Self>) -> impl IntoElement {
        let (tr, curve) = self
            .transient_result
            .clone()
            .unwrap_or((0.0, Rc::new(vec![])));

        v_flex()
            .size_full()
            .gap_2()
            .p_2()
            .child(
                h_flex()
                    .gap_6()
                    .items_center()
                    .child(Label::new("Transient / Step response").font_weight(FontWeight::BOLD))
                    .child(if tr > 0.0 {
                        Label::new(format!("t_r(10–90) = {}", human_time(tr))).font_weight(FontWeight::BOLD)
                    } else {
                        Label::new("фронт не найден").muted()
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .child(TraceCanvas::new(vec![curve])),
            )
    }
}

impl Render for LabApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.tab;
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .child(Label::new("LabBench").font_weight(FontWeight::BOLD))
                    .child(Label::new(self.status_line.clone()).muted()),
            )
            .child(
                h_flex()
                    .px_3()
                    .gap_2()
                    .child(tab_btn("scope", "Oscilloscope", active == Tab::Scope, Tab::Scope))
                    .child(tab_btn("spectrum", "Spectrum", active == Tab::Spectrum, Tab::Spectrum))
                    .child(tab_btn("lcr", "LCR", active == Tab::Lcr, Tab::Lcr))
                    .child(tab_btn("tr", "Transient", active == Tab::Transient, Tab::Transient)),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .map(|el| match self.tab {
                        Tab::Scope => el.child(self.render_scope(cx)),
                        Tab::Spectrum => el.child(self.render_spectrum(cx)),
                        Tab::Lcr => el.child(self.render_lcr(cx)),
                        Tab::Transient => el.child(self.render_transient(cx)),
                    }),
            )
    }
}

fn tab_btn(id: &'static str, label: &'static str, is_active: bool, tab: Tab) -> impl IntoElement {
    Button::new(id)
        .label(label)
        .when(is_active, |b| b.primary())
        .when(!is_active, |b| b.ghost())
        .on_click(move |_, _, cx| {
            // обновим вкладку главного view через глобальный action проще всего —
            // см. Action ниже
            cx.dispatch_action(SwitchTab(tab));
        })
}

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = labbench, no_json)]
struct SwitchTab(Tab);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = labbench, no_json)]
struct SetRate(u8);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = labbench, no_json)]
struct ToggleChannel(usize);

impl LabApp {
    /// Глобальные action'ы кнопок/чекбоксов (см. on_action в main).
    fn on_switch_tab(&mut self, action: &SwitchTab, cx: &mut Context<Self>) {
        self.tab = action.0;
        cx.notify();
    }

    fn on_set_rate(&mut self, action: &SetRate, cx: &mut Context<Self>) {
        self.rate_level = action.0;
        if let Some(t) = &self.transport {
            t.set_rate(action.0);
        }
        self.status_line = format!(
            "fs ≈ {:.0} Гц (rate={})",
            sample_rate(action.0),
            action.0
        )
        .into();
        cx.notify();
    }

    fn on_toggle_channel(&mut self, action: &ToggleChannel, cx: &mut Context<Self>) {
        let ch = action.0;
        if ch < 10 {
            self.ch_enabled[ch] = !self.ch_enabled[ch];
            if let Some(t) = &self.transport {
                // время выборки канала не меняем — берём текущий общий уровень
                t.set_channel(ch as u8, self.ch_enabled[ch], self.rate_level);
            }
        }
        cx.notify();
    }
}

// ============================================================================
// Утилиты отображения величин
// ============================================================================

fn sample_rate(level: u8) -> f32 {
    // SAMPLE_TIME_LABELS из прошивки: 1.5/2.5/4.5/7.5/19.5/61.5/181.5/601.5 циклов
    const ST: [f32; 8] = [1.5, 2.5, 4.5, 7.5, 19.5, 61.5, 181.5, 601.5];
    let st = ST[(level as usize).min(7)];
    // ≈ 72 МГц ядро, ADC-конвертер ~st тактов + overhead; грубая оценка для подписей
    72_000_000.0 / (st + 13.5)
}

fn human_freq(f: f32) -> String {
    if f >= 1e6 {
        format!("{:.2} MHz", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.2} kHz", f / 1e3)
    } else {
        format!("{:.1} Hz", f)
    }
}

fn human_time(t: f32) -> String {
    if t >= 1e-3 {
        format!("{:.2} ms", t * 1e3)
    } else if t >= 1e-6 {
        format!("{:.2} µs", t * 1e6)
    } else {
        format!("{:.1} ns", t * 1e9)
    }
}

fn human_si(v: f64) -> String {
    let (s, suffix) = match v.abs() {
        x if x >= 1e9 => (v / 1e9, "G"),
        x if x >= 1e6 => (v / 1e6, "M"),
        x if x >= 1e3 => (v / 1e3, "k"),
        x if x >= 1.0 => (v, ""),
        x if x >= 1e-3 => (v * 1e3, "m"),
        x if x >= 1e-6 => (v * 1e6, "µ"),
        x if x >= 1e-9 => (v * 1e9, "n"),
        x if x >= 1e-12 => (v * 1e12, "p"),
        _ => (v, ""),
    };
    format!("{s:.3} {suffix}")
}

/// Дешёвый детерминированный «шум» для симулятора (без rand-зависимости).
fn noise(i: usize) -> f32 {
    let x = (i as u32).wrapping_mul(2654435761);
    ((x >> 8) as f32 / 8388608.0) - 1.0
}

fn main() {
    let args: Cli = <Cli as clap::Parser>::parse();

    gpui_kit::Application::new().run(|cx: &mut App| {
        gpui_kit::init(cx);

        // Глобальные обработчики action'ов — маршрутизуются в главный view.
        cx.on_action(|a: &SwitchTab, cx| {
            cx.activate(true);
            if let Some(root) = cx.try_global::<RootView>() {
                root.0.update(cx, |v, cx| v.on_switch_tab(a, cx));
            }
        });
        cx.on_action(|a: &SetRate, cx| {
            if let Some(root) = cx.try_global::<RootView>() {
                root.0.update(cx, |v, cx| v.on_set_rate(a, cx));
            }
        });
        cx.on_action(|a: &ToggleChannel, cx| {
            if let Some(root) = cx.try_global::<RootView>() {
                root.0.update(cx, |v, cx| v.on_toggle_channel(a, cx));
            }
        });

        cx.open_window(WindowOptions::default(), |window, cx| {
            let app = cx.new(|cx| LabApp::new(args.port.clone(), args.baud, args.simulate, window, cx));
            // запомняем корневой view для диспатча экшенов из кнопок
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
