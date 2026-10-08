//! DSP-ядро приборов: всё, что считает LCR/Z/спектр, — чистые функции над
//! `&[f32]`, без зависимостей от gpui и железа. Это позволяет:
//!   1) тестировать алгоритмы на синтетических сигналах (`cargo test`);
//!   2) прогонять весь UI «на симуляторе» (`--simulate`) без STM32;
//!   3) измерять производительность отдельно от рендера (`bench` bin).

use num_complex::Complex64;
use rustfft::FftPlanner;

// ============================================================================
// Осциллограф: триггер + авто-масштаб
// ============================================================================

/// Индекс точки, где сигнал пересекает `level` по фронту, ближайшей к центру
/// окна. Возвращаем сдвиг, чтобы трасска всегда начиналась на фронте —
/// аналог trigger position в «настоящем» осциллографе.
pub fn find_trigger(y: &[f32], level: f32, hysteresis: f32) -> Option<usize> {
    if y.len() < 4 {
        return None;
    }
    let center = y.len() / 2;
    // ищем назад от центра, чтобы стабильная картинка
    for i in (1..center).rev() {
        if y[i - 1] < level - hysteresis && y[i] >= level {
            return Some(i);
        }
    }
    // не нашли — вперёд от центра
    for i in center + 1..y.len() {
        if y[i - 1] < level - hysteresis && y[i] >= level {
            return Some(i);
        }
    }
    None
}

/// Сдвиг буфера так, чтобы точка `idx` стала началом трассы (с циклическим
/// добором из хвоста).
pub fn roll_to(y: &[f32], idx: usize) -> Vec<f32> {
    let n = y.len();
    if idx == 0 || n == 0 {
        return y.to_vec();
    }
    let mut out = Vec::with_capacity(n);
    out.extend_from_slice(&y[idx..]);
    out.extend_from_slice(&y[..idx]);
    out
}

// ============================================================================
// Анализатор спектра: окно + FFT
// ============================================================================

/// Коэффициенты окна Ханна (d³-склейка краёв → минимум спектральных утечек
/// для несинхронизированной дискретизации).
pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (n - 1).max(1) as f64) as f32)
        .collect()
}

/// Односторонний амплитудный спектр в дБFS.
/// Возвращает (частоты_Гц, уровень_дБ) длиной N/2+1.
pub fn amplitude_db(y: &[f32], fs: f32, window: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let n = y.len();
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n.next_power_of_two());

    let win_sum: f64 = window.iter().map(|&w| w as f64).sum();
    let mut buf: Vec<Complex64> = (0..n.next_power_of_two())
        .map(|i| {
            let v = if i < n { y[i] as f64 * window[i] as f64 } else { 0.0 };
            Complex64::new(v, 0.0)
        })
        .collect();
    fft.process(&mut buf);

    let half = n / 2 + 1;
    let mut freqs = Vec::with_capacity(half);
    let mut dbs = Vec::with_capacity(half);
    let df = fs / n as f32;
    for k in 0..half {
        // нормировка: A = 2·|X|/(N·Σw) для полного диапазона ±1.0 = 0 dBFS
        let mag = buf[k].norm() / win_sum * 2.0;
        freqs.push(k as f32 * df);
        dbs.push((20.0 * (mag.max(1e-9) as f32).log10()).max(-120.0));
    }
    (freqs, dbs)
}

/// Пик спектра (частота, дБ) — для отображения «f₀ / уровень» в заголовке.
pub fn spectral_peak(freqs: &[f32], dbs: &[f32]) -> Option<(f32, f32)> {
    let mut best = (0.0f32, -999.0f32);
    for (i, &d) in dbs.iter().enumerate().skip(1) {
        // пропускаем DC (k=0): постоянная составляющая ADC-смещения
        if d > best.1 {
            best = (freqs[i], d);
        }
    }
    if best.1 > -999.0 {
        Some(best)
    } else {
        None
    }
}

// ============================================================================
// LCR / Z-метр: корреляционный (квадратурный) детектор на 1 частоте
// ============================================================================
//
// Метод: генерируем эталонные sin/cos на частоте f, считаем скалярные
// произведения с сигналом V(t) (напряжение на измеряемом элементе, DUT)
// и с опорой I(t) (напряжение на заведомо известном R_sense).
//
// По сути это single-bin DFT обоих каналов и деление комплексных чисел:
// устойчиво к шуму (усреднение по N отсчётов) и не требует полной FFT.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Impedance {
    pub re: f64,
    pub im: f64,
}

impl Impedance {
    pub fn mag(&self) -> f64 {
        (self.re * self.re + self.im * self.im).sqrt()
    }
    /// фаза в градусах: +90 ≈ индуктивность, −90 ≈ ёмкость
    pub fn phase_deg(&self) -> f64 {
        self.im.atan2(self.re).to_degrees()
    }
}

/// Квадратурный анализ одного канала: возвращает (<x·cos>, <x·sin>).
fn quadrature(x: &[f32], cos: &[f32], sin: &[f32]) -> (f64, f64) {
    let n = x.len().min(cos.len()).min(sin.len()).max(1);
    let (mut c, mut s) = (0.0f64, 0.0f64);
    for i in 0..n {
        c += x[i] as f64 * cos[i] as f64;
        s += x[i] as f64 * sin[i] as f64;
    }
    (c / n as f64, s / n as f64)
}

/// Z по двум каналам: `v` — напряжение на DUT, `i_ch` — напряжение на
/// последовательном R_sense (т.е. пропорционально току). Фаза берётся
/// разностью квадратур, амплитуда — отношением модулей.
pub fn measure_z(v: &[f32], i_ch: &[f32], f_hz: f32, fs: f32, r_sense: f64) -> Impedance {
    let n = v.len();
    let mut cos = vec![0f32; n];
    let mut sin = vec![0f32; n];
    for (k, (c, s)) in cos.iter_mut().zip(sin.iter_mut()).enumerate() {
        let ph = std::f64::consts::TAU * k as f64 * f_hz as f64 / fs as f64;
        *c = ph.cos() as f32;
        *s = ph.sin() as f32;
    }
    let (vc, vs) = quadrature(v, &cos, &sin);
    let (ic, is_) = quadrature(i_ch, &cos, &sin);

    let i_mag = (ic * ic + is_ * is_).sqrt();
    if i_mag < 1e-12 {
        return Impedance::default(); // разомкнутая цепь / нет тока
    }
    // V/I как комплексные числа: (vc + j·vs)/(ic + j·is) · R_sense
    let den = ic * ic + is_ * is_;
    let re = (vc * ic + vs * is_) / den;
    let im = (vs * ic - vc * is_) / den;
    Impedance { re: re * r_sense, im: im * r_sense }
}

/// Из Z и частоты вытаскиваем L или C (по знаку реактивной части).
/// Возвращает (значение, единица): ("Гн"/"Ф"). Резистивную часть даёт отдельно.
pub fn lcr(z: Impedance, f_hz: f32) -> (f64, &'static str) {
    let w = 2.0 * std::f64::consts::PI * f_hz as f64;
    if z.im >= 0.0 {
        (z.im / w, "L, Гн")
    } else {
        (-1.0 / (w * z.im), "C, Ф")
    }
}

// ============================================================================
// Переходные характеристики (Z-метр в режиме TDR/step)
// ============================================================================

/// Оценка времени установления 10%→90% по фронтy step-ответа.
/// Простая, но честная метрика для «переходных характеристик»: для RC-цепи
/// t_r ≈ 2.2·τ.
pub fn rise_time_10_90(y: &[f32], fs: f32) -> Option<f32> {
    if y.len() < 8 {
        return None;
    }
    let lo = y[..y.len() / 8].iter().copied().fold(f32::INFINITY, f32::min);
    let hi = y[y.len() - y.len() / 8..].iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let span = hi - lo;
    if span.abs() < 0.02 {
        return None; // нет перехода — нечего мерить
    }
    let t10 = lo + span * 0.1;
    let t90 = lo + span * 0.9;
    let rising = hi > lo;
    let cross = |level: f32| -> Option<usize> {
        y.windows(2).position(|w| {
            if rising {
                w[0] < level && w[1] >= level
            } else {
                w[0] > level && w[1] <= level
            }
        })
        .map(|i| i + 1)
    };
    match (cross(t10), cross(t90)) {
        (Some(a), Some(b)) if b > a => Some((b - a) as f32 / fs),
        _ => None,
    }
}

// ============================================================================
// Тесты на синтетических сигналах
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, f: f32, fs: f32, amp: f32, phase: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * ((std::f64::consts::TAU * i as f64 * f as f64 / fs as f64) + phase as f64).sin() as f32)
            .collect()
    }

    #[test]
    fn fft_finds_tone_frequency() {
        let (fs, f0, n) = (72_000.0f32, 5_000.0f32, 4096);
        let y = sine(n, f0, fs, 0.5, 0.0);
        let w = hann(n);
        let (fr, db) = amplitude_db(&y, fs, &w);
        let (fpk, dpk) = spectral_peak(&fr, &db).unwrap();
        assert!((fpk - f0).abs() < fs / n as f32 * 1.5, "peak at {fpk}, want {f0}");
        // тон амплитуды 0.5 → около −6 dBFS
        assert!(dpk > -8.0 && dpk < -4.0, "level {dpk} dB");
    }

    #[test]
    fn trigger_locks_on_front() {
        let y = sine(1024, 100.0, 10_000.0, 0.8, 0.0);
        let idx = find_trigger(&y, 0.0, 0.05).unwrap();
        let r = roll_to(&y, idx);
        assert!(r[0] >= 0.0 && r[1] > r[0]); // старт на фронте
    }

    #[test]
    fn z_meter_recovers_rc_load() {
        // Цепь: R=1000 Ом последовательно с C=100 нФ при f=1591.5 Гц
        // → Xc = 1/(2πfC) ≈ 1000 Ом, т.е. Z_dut = 1000 − j1000 Ом.
        // Ток через цепь — единичная синусоида; напряжение на DUT опережает ток
        // на фазу Z. В качестве «R_sense» берём 1 Ом, чтобы масштаб совпал.
        let (fs, f) = (100_000.0f32, 1591.5f32);
        let n = 8192;
        let r_sense = 1.0f64;

        let i_ch = sine(n, f, fs, 0.4, 0.0); // ~ ток
        let z_re = 1000.0f64;
        let z_im = -1000.0f64;
        let mag = (z_re * z_re + z_im * z_im).sqrt();
        let phz = z_im.atan2(z_re);
        let v = sine(n, f, fs, (0.4 * mag) as f32, phz as f32); // ~ I·|Z|

        let z = measure_z(&v, &i_ch, f, fs, r_sense);
        assert!((z.re - 1000.0).abs() / 1000.0 < 0.05, "Re = {}", z.re);
        assert!((z.im + 1000.0).abs() / 1000.0 < 0.05, "Im = {}", z.im);
        assert!((z.phase_deg() + 45.0).abs() < 3.0, "phase {}", z.phase_deg());

        let (val, unit) = lcr(z, f);
        assert_eq!(unit, "C, Ф");
        assert!(val > 80e-9 && val < 120e-9, "C = {val:e}");
    }

    #[test]
    fn z_meter_recovers_rl_load() {
        // R=220 Ом + L=10 мГн при f=10 кГц → X_L = 628 Ом
        let (fs, f) = (200_000.0f32, 10_000.0f32);
        let n = 8192;
        let (z_re, z_im) = (220.0f64, 2.0 * std::f64::consts::PI * 10e-3 * 10e3);
        let mag = (z_re * z_re + z_im * z_im).sqrt();
        let i_ch = sine(n, f, fs, 0.3, 0.0);
        let v = sine(n, f, fs, (0.3 * mag) as f32, z_im.atan2(z_re) as f32);

        let z = measure_z(&v, &i_ch, f, fs, 1.0);
        assert!((z.re - 220.0).abs() < 20.0, "Re = {}", z.re);
        let (l, unit) = lcr(z, f);
        assert_eq!(unit, "L, Гн");
        assert!((l - 10e-3).abs() / 10e-3 < 0.05, "L = {l:e}");
    }

    #[test]
    fn rise_time_on_exponential_step() {
        let fs = 100_000.0f32;
        let tau = 1e-3f32; // τ = 1 мс → t_r(10-90) ≈ 2.2 мс
        let y: Vec<f32> = (0..8192).map(|i| 1.0 - (-i as f32 / (fs * tau)).exp()).collect();
        let tr = rise_time_10_90(&y, fs).unwrap();
        assert!((tr - 2.2e-3).abs() < 0.3e-3, "t_r = {tr:e}");
    }
}
