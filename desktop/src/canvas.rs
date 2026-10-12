use std::rc::Rc;

use gpui_kit::*;

/// Цвета каналов — те же, что `CHANNEL_COLORS` в Python-версии.
/// Инициализируем через блок static { ... }, потому что hsla() не const fn.
pub static CHANNEL_COLORS: [Hsla; 10] = {
    let mut colors = [
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
        Hsla { h: 0.0, s: 0.0, l: 0.0, a: 0.0 },
    ];
    colors[0] = hsla(0.97, 0.80, 0.50, 1.0); // #e6194b
    colors[1] = hsla(0.36, 0.42, 0.47, 1.0); // #3cb44b
    colors[2] = hsla(0.63, 0.65, 0.55, 1.0); // #4363d8
    colors[3] = hsla(0.07, 0.90, 0.58, 1.0); // #f58231
    colors[4] = hsla(0.78, 0.72, 0.41, 1.0); // #911eb4
    colors[5] = hsla(0.52, 0.42, 0.61, 1.0); // #42d4f4
    colors[6] = hsla(0.87, 0.79, 0.57, 1.0); // #f032e6
    colors[7] = hsla(0.20, 0.83, 0.60, 1.0); // #bfef45
    colors[8] = hsla(0.95, 0.85, 0.86, 1.0); // #fabed4
    colors[9] = hsla(0.49, 0.37, 0.43, 1.0); // #469990
    colors
};

/// Полуразмах АЦП в вольтах: нормировка прошивки `raw/2047.5 - 1.0`
/// (см. `LabApp::push_frame`) при Vdda=3.3 В даёт ±1.65 В.
pub const UNIT_V: f32 = 1.65;

/// Номер цвета канала (для кликабельного маркера в панели).
pub fn channel_color(ch: usize) -> Hsla {
    CHANNEL_COLORS[ch % CHANNEL_COLORS.len()]
}

#[derive(Clone)]
pub struct TraceCanvas {
    pub channels: Vec<Rc<Vec<f32>>>,
    pub grid_divs_x: usize,
    pub grid_divs_y: usize,
    pub trigger_level: Option<f32>,
    pub line_width: f32,
    /// Вольт на деление (вертикальная развёртка).
    pub v_per_div: f32,
    /// Миллисекунд на деление (горизонтальная развёртка), только для подписей.
    pub time_per_div_ms: f32,
    /// Подсветка маркера канала (индекс) — для подсветки активного канала.
    pub highlight: Option<usize>,
    /// Смещение трассы канала по вертикали, вольт (выровнено с `channels`).
    pub offsets: Vec<f32>,
    /// Две вертикальные метки-курсора как доли ширины [0..1]: (A, B).
    pub cursors: Option<(f32, f32)>,
}

impl TraceCanvas {
    pub fn new(channels: Vec<Rc<Vec<f32>>>) -> Self {
        Self {
            channels,
            grid_divs_x: 10,
            grid_divs_y: 8,
            trigger_level: None,
            line_width: 1.0,
            v_per_div: 0.5,
            time_per_div_ms: 1.0,
            highlight: None,
            offsets: Vec::new(),
            cursors: None,
        }
    }

    pub fn trigger_level(mut self, level: Option<f32>) -> Self {
        self.trigger_level = level;
        self
    }

    pub fn v_per_div(mut self, v: f32) -> Self {
        self.v_per_div = v.max(1e-4);
        self
    }

    pub fn time_per_div_ms(mut self, t: f32) -> Self {
        self.time_per_div_ms = t;
        self
    }

    /// Вертикальные смещения трасс каналов (вольт), по одному на канал.
    pub fn offsets(mut self, offs: Vec<f32>) -> Self {
        self.offsets = offs;
        self
    }

    /// Подсветить канал (индекс позиции в `channels`).
    pub fn highlight(mut self, ch: Option<usize>) -> Self {
        self.highlight = ch;
        self
    }

    /// Показать пару курсоров (доли ширины 0..1).
    pub fn cursors(mut self, c: Option<(f32, f32)>) -> Self {
        self.cursors = c;
        self
    }
}

impl Element for TraceCanvas {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> { None }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> { None }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: Size::full(),
            ..Default::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = bounds.origin;
        let size = bounds.size;
        let divs_x = self.grid_divs_x.max(1);
        let divs_y = self.grid_divs_y.max(1);
        let trig = self.trigger_level;
        let lw = self.line_width;
        let v_per_div = self.v_per_div.max(1e-6);

        window.paint_quad(fill(bounds, SCOPE_BG));

        let per_div_px = size.height / divs_y as f32;
        // Нормированное значение (в единицах UNIT_V) → пиксель Y, со смещением канала (В).
        let my = |v: f32, off_v: f32| -> Pixels {
            let volts = v * UNIT_V + off_v;
            origin.y + size.height * 0.5 - (volts / v_per_div) * per_div_px
        };

        // --- Сетка (минор/мажор) ---
        for i in 0..=divs_x {
            let x = origin.x + size.width * (i as f32 / divs_x as f32);
            let major = i % 5 == 0;
            let color = if major { GRID_MAJOR } else { GRID_MINOR };
            stroke_line(window, Point::new(x, origin.y), Point::new(x, origin.y + size.height), color, 1.0);
        }
        for j in 0..=divs_y {
            let y = origin.y + size.height * (j as f32 / divs_y as f32);
            let major = j % 4 == 0;
            let color = if major { GRID_MAJOR } else { GRID_MINOR };
            stroke_line(window, Point::new(origin.x, y), Point::new(origin.x + size.width, y), color, 1.0);
        }

        // --- Оцифровка осей (для измерений) ---
        let y_mid = divs_y / 2;
        for j in (0..=divs_y).step_by((divs_y / 2).max(1)) {
            let volts = (y_mid as f32 - j as f32) * v_per_div;
            let y = origin.y + size.height * (j as f32 / divs_y as f32);
            draw_label(window, cx, &fmt_volts(volts), Point::new(origin.x + px(3.), y + px(1.)), AXIS_TEXT);
        }
        let x_mid = divs_x / 2;
        for i in (0..=divs_x).step_by((divs_x / 2).max(1)) {
            let tms = (i as f32 - x_mid as f32) * self.time_per_div_ms;
            let x = origin.x + size.width * (i as f32 / divs_x as f32);
            let ly = origin.y + size.height - px(13.);
            draw_label(window, cx, &fmt_ms(tms), Point::new(x + px(3.), ly), AXIS_TEXT);
        }

        // Уровень триггера (пунктир)
        if let Some(level) = trig {
            let y = my(level, 0.0);
            let mut b = PathBuilder::stroke(px(1.0)).dash_array(&[px(4.), px(3.)]);
            b.move_to(Point::new(origin.x, y));
            b.line_to(Point::new(origin.x + size.width, y));
            if let Ok(p) = b.build() {
                window.paint_path(p, hsla(0.13, 0.9, 0.6, 0.8));
            }
        }

        let n_px = (size.width.as_f32().round()).max(1.0) as usize;
        for (ci, ch) in self.channels.iter().enumerate() {
            let color = channel_color(ci);
            let off = self.offsets.get(ci).copied().unwrap_or(0.0);
            let lw_ch = if self.highlight == Some(ci) { lw + 1.2 } else { lw };
            let ys = ch.as_slice();
            let n = ys.len();
            if n < 2 {
                continue;
            }

            if n <= n_px * 2 {
                let mut b = PathBuilder::stroke(px(lw_ch));
                for (i, &v) in ys.iter().enumerate() {
                    let pt = Point::new(
                        origin.x + size.width * (i as f32 / (n - 1) as f32),
                        my(v, off),
                    );
                    if i == 0 {
                        b.move_to(pt);
                    } else {
                        b.line_to(pt);
                    }
                }
                if let Ok(p) = b.build() {
                    window.paint_path(p, color);
                }
            } else {
                let mut b = PathBuilder::stroke(px(lw_ch));
                let scale = n as f32 / n_px as f32;
                for col in 0..n_px {
                    let i0 = (col as f32 * scale) as usize;
                    let i1 = (((col + 1) as f32 * scale) as usize)
                        .min(n.saturating_sub(1))
                        .max(i0 + 1);
                    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
                    for &v in &ys[i0..i1] {
                        if v < lo { lo = v; }
                        if v > hi { hi = v; }
                    }
                    let x = origin.x + size.width * (col as f32 / n_px as f32);
                    b.move_to(Point::new(x, my(lo, off)));
                    b.line_to(Point::new(x, my(hi, off)));
                }
                if let Ok(p) = b.build() {
                    window.paint_path(p, color);
                }
            }
        }

        // --- Курсоры A/B (вертикальные метки) ---
        if let Some((a, b)) = self.cursors {
            for (frac, label, col) in [
                (a, "A", hsla(0.13, 0.9, 0.60, 0.95)),
                (b, "B", hsla(0.02, 0.9, 0.60, 0.95)),
            ] {
                let x = origin.x + size.width * frac.clamp(0.0, 1.0);
                stroke_line(window, Point::new(x, origin.y), Point::new(x, origin.y + size.height), col, 1.0);
                draw_label(window, cx, label, Point::new(x + px(3.), origin.y + px(2.)), col);
            }
        }
    }
}

impl IntoElement for TraceCanvas {
    type Element = Self;
    fn into_element(self) -> Self::Element { self }
}

/// Универсальный линейный график (спектр/переходный процесс): тонкая линия,
/// сетка и оцифровка осей. Заменяет `LineChart` из gpui-component, у которого
/// толщина линии жёстко задана 2 px.
#[derive(Clone)]
pub struct PlotCanvas {
    pub points: Vec<(f32, f32)>,
    pub grid_divs_x: usize,
    pub grid_divs_y: usize,
    pub line_color: Hsla,
    pub line_width: f32,
    pub x_fmt: Rc<dyn Fn(f32) -> String>,
    pub y_fmt: Rc<dyn Fn(f32) -> String>,
    /// Фиксированный диапазон Y (min, max); иначе — по данным.
    pub y_range: Option<(f32, f32)>,
    /// Вертикальная метка (например, найденный пик спектра), в координатах X.
    pub marker_x: Option<f32>,
}

impl PlotCanvas {
    pub fn new(
        points: Vec<(f32, f32)>,
        x_fmt: Rc<dyn Fn(f32) -> String>,
        y_fmt: Rc<dyn Fn(f32) -> String>,
    ) -> Self {
        Self {
            points,
            grid_divs_x: 10,
            grid_divs_y: 8,
            line_color: hsla(0.58, 0.65, 0.58, 1.0),
            line_width: 1.0,
            x_fmt,
            y_fmt,
            y_range: None,
            marker_x: None,
        }
    }

    pub fn line_color(mut self, c: Hsla) -> Self { self.line_color = c; self }
    pub fn y_range(mut self, lo: f32, hi: f32) -> Self { self.y_range = Some((lo, hi)); self }

    /// Вертикальная метка (например, найденный пик спектра), в координатах X.
    pub fn marker_x(mut self, x: f32) -> Self { self.marker_x = Some(x); self }
}

impl Element for PlotCanvas {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> { None }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> { None }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: Size::full(),
            ..Default::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = bounds.origin;
        let size = bounds.size;
        let divs_x = self.grid_divs_x.max(1);
        let divs_y = self.grid_divs_y.max(1);

        window.paint_quad(fill(bounds, SCOPE_BG));

        let n = self.points.len();
        if n < 2 {
            draw_label(window, cx, "нет данных", Point::new(origin.x + px(12.), origin.y + px(12.)), AXIS_TEXT);
            return;
        }
        let (mut xmin, mut xmax) = (f32::INFINITY, f32::NEG_INFINITY);
        for &(x, _) in &self.points {
            if x < xmin { xmin = x; }
            if x > xmax { xmax = x; }
        }
        let (ymin, ymax) = match self.y_range {
            Some(r) => r,
            None => {
                let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
                for &(_, y) in &self.points {
                    if y < lo { lo = y; }
                    if y > hi { hi = y; }
                }
                if !lo.is_finite() || !hi.is_finite() {
                    (lo, hi)
                } else if (hi - lo).abs() < 1e-9 {
                    (lo - 1.0, hi + 1.0)
                } else {
                    (lo, hi)
                }
            }
        };
        let xspan = (xmax - xmin).max(1e-9);
        let yspan = (ymax - ymin).max(1e-9);
        let mx = |x: f32| -> Pixels { origin.x + size.width * ((x - xmin) / xspan) };
        let my = |y: f32| -> Pixels { origin.y + size.height * (1.0 - (y - ymin) / yspan) };

        for i in 0..=divs_x {
            let x = origin.x + size.width * (i as f32 / divs_x as f32);
            let major = i % 5 == 0;
            stroke_line(window, Point::new(x, origin.y), Point::new(x, origin.y + size.height),
                if major { GRID_MAJOR } else { GRID_MINOR }, 1.0);
        }
        for j in 0..=divs_y {
            let y = origin.y + size.height * (j as f32 / divs_y as f32);
            let major = j % 4 == 0;
            stroke_line(window, Point::new(origin.x, y), Point::new(origin.x + size.width, y),
                if major { GRID_MAJOR } else { GRID_MINOR }, 1.0);
        }

        for j in (0..=divs_y).step_by((divs_y / 2).max(1)) {
            let val = ymax - yspan * (j as f32 / divs_y as f32);
            let y = origin.y + size.height * (j as f32 / divs_y as f32);
            draw_label(window, cx, &(self.y_fmt)(val), Point::new(origin.x + px(3.), y + px(1.)), AXIS_TEXT);
        }
        for i in (0..=divs_x).step_by((divs_x / 2).max(1)) {
            let val = xmin + xspan * (i as f32 / divs_x as f32);
            let x = origin.x + size.width * (i as f32 / divs_x as f32);
            let ly = origin.y + size.height - px(13.);
            draw_label(window, cx, &(self.x_fmt)(val), Point::new(x + px(3.), ly), AXIS_TEXT);
        }

        let mut b = PathBuilder::stroke(px(self.line_width));
        for (i, &(x, y)) in self.points.iter().enumerate() {
            let pt = Point::new(mx(x), my(y));
            if i == 0 {
                b.move_to(pt);
            } else {
                b.line_to(pt);
            }
        }
        if let Ok(p) = b.build() {
            window.paint_path(p, self.line_color);
        }

        // Вертикальная метка (пик) — поверх графика.
        if let Some(mx_v) = self.marker_x {
            if mx_v.is_finite() {
                let x = mx(mx_v);
                if x >= origin.x && x <= origin.x + size.width {
                    stroke_line(
                        window,
                        Point::new(x, origin.y),
                        Point::new(x, origin.y + size.height),
                        hsla(0.13, 0.9, 0.6, 0.9),
                        1.0,
                    );
                }
            }
        }
    }
}

impl IntoElement for PlotCanvas {
    type Element = Self;
    fn into_element(self) -> Self::Element { self }
}

// --- Общие цветовые константы канвы (в тон «серо-стальной» палитре) ---
const SCOPE_BG: Hsla = Hsla { h: 0.60, s: 0.06, l: 0.075, a: 1.0 };
const GRID_MINOR: Hsla = Hsla { h: 0.60, s: 0.03, l: 0.34, a: 0.70 };
const GRID_MAJOR: Hsla = Hsla { h: 0.60, s: 0.03, l: 0.52, a: 0.90 };
const AXIS_TEXT: Hsla = Hsla { h: 0.58, s: 0.10, l: 0.62, a: 1.0 };

fn stroke_line(window: &mut Window, a: Point<Pixels>, b: Point<Pixels>, color: Hsla, w: f32) {
    let mut builder = PathBuilder::stroke(px(w));
    builder.move_to(a);
    builder.line_to(b);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn draw_label(window: &mut Window, cx: &mut App, text: &str, pos: Point<Pixels>, color: Hsla) {
    if text.is_empty() {
        return;
    }
    let font = window.text_style().font();
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shared = SharedString::from(text.to_string());
    let line = window.text_system().shape_line(shared, px(10.0), &[run], None);
    let _ = line.paint(pos, px(12.0), TextAlign::Left, None, window, cx);
}

fn fmt_volts(v: f32) -> String {
    if v.abs() < 1e-6 {
        "0".to_string()
    } else if v.abs() < 1.0 {
        format!("{:.2} В", v)
    } else {
        format!("{:.1} В", v)
    }
}

fn fmt_ms(t: f32) -> String {
    let a = t.abs();
    if a < 1e-6 {
        "0".to_string()
    } else if a < 0.01 {
        format!("{:.0} нс", t * 1e6)
    } else if a < 1.0 {
        format!("{:.0} мкс", t * 1e3)
    } else {
        format!("{:.1} мс", t)
    }
}
