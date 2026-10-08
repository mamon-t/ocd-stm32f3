//! Кастомный canvas-элемент трассы — «самое сложное» по плану.
//!
//! Принимает `&[Rc<Vec<f32>>]` (каналы, значения в [-1..1]) и рисует ломаную
//! через gpui `canvas()` + `PathBuilder::stroke()` — ровно так же, как это
//! делает официальный пример `examples/brush` из gpui-kit. Никакого растрового
//! прокси (plotters → image → текстура) не понадобилось: векторные quads
//! собираются на GPU-бэкенде GPUI быстрее, чем matplotlib перерисовывает Agg.
//!
//! Приём для 60 fps при длинных трассах: если точек больше, чем пикселей по
//! ширине, рисуем min/max-огибающую (по одному вертикальному сегменту на
//! пиксель-колонку). Картинка честнее «прореженной» линии, а стоимость
//! рендера ограничена шириной окна (~1200 сегментов вместо 100 000).

use std::rc::Rc;

use gpui_kit::*;

/// Цвета каналов — те же, что `CHANNEL_COLORS` в Python-версии.
pub const CHANNEL_COLORS: [Hsla; 10] = [
    hsl(0.97, 0.80, 0.50), // #e6194b
    hsl(0.36, 0.42, 0.47), // #3cb44b
    hsl(0.63, 0.65, 0.55), // #4363d8
    hsl(0.07, 0.90, 0.58), // #f58231
    hsl(0.78, 0.72, 0.41), // #911eb4
    hsl(0.52, 0.42, 0.61), // #42d4f4
    hsl(0.87, 0.79, 0.57), // #f032e6
    hsl(0.20, 0.83, 0.60), // #bfef45
    hsl(0.95, 0.85, 0.86), // #fabed4
    hsl(0.49, 0.37, 0.43), // #469990
];

#[inline]
fn hsl(h: f32, s: f32, l: f32) -> Hsla {
    hsla(h, s, l, 1.0)
}

/// Элемент: сетка + N трасс. Дешев клонируется (внутри только Rc).
#[derive(Clone)]
pub struct TraceCanvas {
    pub channels: Rc<Vec<Rc<Vec<f32>>>>,
    /// Подписи к сетке по X (например "1ms/div") — для заголовка осциллографа
    pub grid_divs_x: usize,
    pub grid_divs_y: usize,
    /// Горизонтальная линия уровня триггера (в тех же координатах, что данные)
    pub trigger_level: Option<f32>,
    pub line_width: f32,
}

impl TraceCanvas {
    pub fn new(channels: Vec<Rc<Vec<f32>>>) -> Self {
        Self {
            channels: Rc::new(channels),
            grid_divs_x: 10,
            grid_divs_y: 8,
            trigger_level: None,
            line_width: 1.5,
        }
    }

    pub fn trigger_level(mut self, level: Option<f32>) -> Self {
        self.trigger_level = level;
        self
    }
}

impl Element for TraceCanvas {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Занимаем всю предложенную область: родитель задаёт размер канвы.
        let style = StyleRefinement {
            size: SizeStyleRefinement {
                width: Some(DependsLayout::def(|s| s.max_width)),
                height: Some(DependsLayout::def(|s| s.max_height)),
            },
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _rstate: &mut Self::RequestLayoutState,
        _pstate: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = bounds.origin;
        let size = bounds.size;
        let channels = self.channels.clone();
        let divs_x = self.grid_divs_x;
        let divs_y = self.grid_divs_y;
        let trig = self.trigger_level;
        let lw = self.line_width;

        // фон канвы
        window.paint_quad(fill(bounds, hsla(0.64, 0.40, 0.12, 1.0)));

        // --- сетка -----------------------------------------------------------
        let grid = hsla(0.62, 0.30, 0.35, 0.35);
        let grid_major = hsla(0.62, 0.30, 0.45, 0.55);
        for i in 0..=divs_x {
            let x = origin.x + size.width * (i as f32 / divs_x as f32);
            let color = if i == divs_x / 2 { grid_major } else { grid };
            stroke_line(window, Point::new(x, origin.y), Point::new(x, origin.y + size.height), color, 1.0);
        }
        for j in 0..=divs_y {
            let y = origin.y + size.height * (j as f32 / divs_y as f32);
            let color = if j == divs_y / 2 { grid_major } else { grid };
            stroke_line(window, Point::new(origin.x, y), Point::new(origin.x + size.width, y), color, 1.0);
        }

        // --- уровень триггера ------------------------------------------------
        if let Some(level) = trig {
            let y = map_y(origin, size.height, level);
            let mut b = PathBuilder::stroke(px(1.0)).dash_array(&[px(4.), px(3.)]);
            b.move_to(Point::new(origin.x, y));
            b.line_to(Point::new(origin.x + size.width, y));
            if let Ok(p) = b.build() {
                window.paint_path(p, hsla(0.13, 0.9, 0.6, 0.8));
            }
        }

        // --- трассы ------------------------------------------------------------
        let n_px = size.width.0.round().max(1.0) as usize;
        for (ci, ch) in channels.iter().enumerate() {
            let color = CHANNEL_COLORS[ci % CHANNEL_COLORS.len()];
            let ys = ch; // &[f32]
            let n = ys.len();
            if n < 2 {
                continue;
            }

            if n <= n_px * 2 {
                // мало точек — обычная ломаная
                let mut b = PathBuilder::stroke(px(lw));
                for (i, &v) in ys.iter().enumerate() {
                    let pt = Point::new(
                        origin.x + size.width * (i as f32 / (n - 1) as f32),
                        map_y(origin, size.height, v),
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
                // много точек — min/max огибающая на колонку
                let mut b = PathBuilder::stroke(px(lw));
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
                    b.move_to(Point::new(x, map_y(origin, size.height, lo)));
                    b.line_to(Point::new(x, map_y(origin, size.height, hi)));
                }
                if let Ok(p) = b.build() {
                    window.paint_path(p, color);
                }
            }
        }
    }
}

impl IntoElement for TraceCanvas {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

#[inline]
fn map_y(origin: Point<Pixels>, height: Pixels, v: f32) -> Pixels {
    // v ∈ [-1..1] → экранный Y (сверху вниз)
    origin.y + height * (0.5 - v.clamp(-1.0, 1.0) * 0.5)
}

fn stroke_line(window: &mut Window, a: Point<Pixels>, b: Point<Pixels>, color: Hsla, w: f32) {
    let mut builder = PathBuilder::stroke(px(w));
    builder.move_to(a);
    builder.line_to(b);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}
