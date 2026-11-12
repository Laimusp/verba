//! Знак Verba и значки из него — рисуются кодом по расстояниям до фигур (сглаженные края при любом DPI).
//!
//! Знак: три полосы голоса убывают к текстовому курсору — «Verba volant, scripta manent»: слова улетают,
//! написанное остаётся. Те же числа — у картинок README (`docs/readme/src/build-svg.py`, `intro.html`).
//! - Трей (CLAUDE.md «Стиль интерфейса»): заливка без обводок, состояние — цветом полос; цвет глифа — по теме
//!   панели задач (белый на тёмной, почти чёрный на светлой).
//! - Иконка exe (рабочий стол, панель задач, проводник) — коралловая плитка `render_app`; файл `assets/verba.ico`
//!   собирает и сверяет `tests/app_icon.rs`, в exe его вшивает `build.rs`.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IconState {
    Ready,
    Paused,
    Recording,
    Transcribing,
    Error,
}

pub type Rgb = (u8, u8, u8);

const RED: Rgb = (0xE5, 0x48, 0x4D);
const AMBER: Rgb = (0xF5, 0xA5, 0x24);
/// плитка иконки — тот же красный, что у записи в трее
pub const CORAL: Rgb = RED;
/// курсор на плитке
pub const INK: Rgb = (0x1B, 0x19, 0x17);
const WHITE: Rgb = (0xFF, 0xFF, 0xFF);

// Сетка знака 120×120 (как у SVG в README): курсор высотой 88, полосы голоса 72 → 50 → 28.
const VOICE_H: [f32; 3] = [72.0, 50.0, 28.0];
const BAR_W: f32 = 14.0;
const BAR_GAP: f32 = 8.0;
const CARET_GAP: f32 = 11.0;
const CARET_W: f32 = 8.0;
const CARET_H: f32 = 88.0;

/// Капсула в пикселях холста: прямоугольник со скруглёнными короткими сторонами.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pill {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Pill {
    /// Расстояние со знаком от точки до капсулы (внутри — меньше нуля), в пикселях.
    fn dist(&self, px: f32, py: f32) -> f32 {
        let r = self.w.min(self.h) / 2.0;
        let (cx, cy) = (self.x + self.w / 2.0, self.y + self.h / 2.0);
        let (hx, hy) = (self.w / 2.0 - r, self.h / 2.0 - r);
        seg(px, py, cx - hx, cy - hy, cx + hx, cy + hy) - r
    }
}

/// Полосы голоса и курсор на холсте `canvas`×`canvas`, курсор высотой ≈ `height` пикселей, всё по центру.
/// Ширины, просветы и края — целые пиксели, высота той же чётности, что холст: на 16–32 px полосы не мылятся.
pub fn layout(canvas: u32, height: f32) -> ([Pill; 3], Pill) {
    let s = canvas as f32;
    let k = height / CARET_H;
    let px = |v: f32| (v * k).round().max(1.0);
    let (bw, gap, cgap, cw) = (px(BAR_W), px(BAR_GAP), px(CARET_GAP), px(CARET_W));
    // высота той же чётности, что холст: капсула встаёт ровно по центру, без полупикселя
    let snap = |h: f32| {
        let odd = canvas % 2;
        let n = (((h * k) - odd as f32) / 2.0).round() * 2.0 + odd as f32;
        n.max(2.0 + odd as f32)
    };
    let x0 = ((s - (3.0 * bw + 2.0 * gap + cgap + cw)) / 2.0).round();
    let pill = |x: f32, w: f32, h: f32| Pill { x, y: (s - h) / 2.0, w, h };
    let bars = [0, 1, 2].map(|i| pill(x0 + i as f32 * (bw + gap), bw, snap(VOICE_H[i])));
    let caret = pill(x0 + 3.0 * bw + 2.0 * gap + cgap, cw, snap(CARET_H));
    (bars, caret)
}

fn length(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

/// Расстояние до отрезка AB.
fn seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    length(px - ax - t * dx, py - ay - t * dy)
}

/// Покрытие пикселя фигурой по расстоянию в пикселях: 1 внутри, 0 снаружи, сглаженный край в один пиксель.
fn cover(d_px: f32) -> f32 {
    (0.5 - d_px).clamp(0.0, 1.0)
}

/// Покрытие полосами голоса и курсором в точке (полосы и курсор не пересекаются).
fn mark_at(bars: &[Pill; 3], caret: &Pill, x: f32, y: f32) -> (f32, f32) {
    let voice = bars.iter().map(|b| cover(b.dist(x, y))).fold(0.0, f32::max);
    (voice, cover(caret.dist(x, y)))
}

fn put(out: &mut [u8], size: u32, px: u32, py: u32, c: Rgb, alpha: f32) {
    let i = ((py * size + px) * 4) as usize;
    out[i] = c.0;
    out[i + 1] = c.1;
    out[i + 2] = c.2;
    out[i + 3] = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
}

/// Значок трея: RGBA, строки сверху вниз, без предумножения альфы.
pub fn render(state: IconState, size: u32, dark_taskbar: bool) -> Vec<u8> {
    let fg: Rgb = if dark_taskbar { (0xFF, 0xFF, 0xFF) } else { (0x1C, 0x1C, 0x1E) };
    let s = size as f32;
    let (bars, caret) = layout(size, s * 0.875);
    let mut out = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let (voice, cur) = mark_at(&bars, &caret, x, y);
            let glyph = voice.max(cur);
            // запись и распознавание красят голос, курсор остаётся цветом панели
            let tinted = |c: Rgb| if voice > cur { (c, voice) } else { (fg, cur) };
            let (color, alpha) = match state {
                IconState::Ready => (fg, glyph),
                IconState::Recording => tinted(RED),
                IconState::Transcribing => tinted(AMBER),
                IconState::Paused => {
                    // приглушённый знак, перечёркнутый косой чертой с просветом вокруг неё
                    let slash = seg(x, y, 0.16 * s, 0.10 * s, 0.84 * s, 0.90 * s);
                    let line = cover(slash - 0.055 * s);
                    let gap = cover(slash - 0.13 * s);
                    (fg, (glyph * 0.45 * (1.0 - gap)).max(line))
                }
                IconState::Error => {
                    // красная точка в углу, вокруг неё просвет
                    let dot = length(x - 0.78 * s, y - 0.78 * s);
                    let badge = cover(dot - 0.2 * s);
                    let base = glyph * (1.0 - cover(dot - 0.29 * s));
                    if badge > 0.0 {
                        (RED, badge.max(base))
                    } else {
                        (fg, base)
                    }
                }
            };
            put(&mut out, size, px, py, color, alpha);
        }
    }
    out
}

/// Плитка иконки приложения: сторона плитки в пикселях и знак на ней.
fn app_layout(size: u32) -> (f32, [Pill; 3], Pill) {
    // поле вокруг плитки: 0 до 31 px, 1 px на 32–63, … 8 px на 256
    let tile = size as f32 - 2.0 * (size as f32 / 32.0).floor();
    // на мелких размерах знак крупнее, иначе полосы выходят в 1 px; от 48 px — 0,78 сетки знака (88/120 × 0,78)
    let fill = 0.72 - (0.72 - 0.572) * ((tile - 16.0) / 32.0).clamp(0.0, 1.0);
    let (bars, caret) = layout(size, tile * fill);
    (tile, bars, caret)
}

/// Иконка приложения: коралловая плитка, белые полосы голоса, тёмный курсор. RGBA, строки сверху вниз.
pub fn render_app(size: u32) -> Vec<u8> {
    let s = size as f32;
    let (tile, bars, caret) = app_layout(size);
    let radius = tile * 0.225;
    let mut out = vec![0u8; (size * size * 4) as usize];
    let (c, half) = (s / 2.0, tile / 2.0 - radius);
    for py in 0..size {
        for px in 0..size {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let (qx, qy) = ((x - c).abs() - half, (y - c).abs() - half);
            let d = length(qx.max(0.0), qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
            let plate = cover(d);
            if plate <= 0.0 {
                continue;
            }
            let (voice, cur) = mark_at(&bars, &caret, x, y);
            let base = 1.0 - voice - cur;
            let mix = |a: u8, b: u8, i: u8| a as f32 * base + b as f32 * voice + i as f32 * cur;
            let color = (
                mix(CORAL.0, WHITE.0, INK.0).round() as u8,
                mix(CORAL.1, WHITE.1, INK.1).round() as u8,
                mix(CORAL.2, WHITE.2, INK.2).round() as u8,
            );
            put(&mut out, size, px, py, color, plate);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    /// пиксель в середине курсора
    fn caret_mid(size: u32, height: f32) -> (u32, u32) {
        let (_, c) = layout(size, height);
        ((c.x + c.w / 2.0) as u32, size / 2)
    }

    #[test]
    fn layout_sits_on_the_pixel_grid() {
        for size in [16, 20, 24, 32, 40, 48, 64, 256] {
            let (bars, caret) = layout(size, size as f32 * 0.875);
            let all = [bars[0], bars[1], bars[2], caret];
            for p in all {
                assert_eq!(p.x.fract(), 0.0, "{size}px: x {p:?}");
                assert_eq!(p.w.fract(), 0.0, "{size}px: w {p:?}");
                assert_eq!(p.y.fract(), 0.0, "{size}px: y {p:?}");
                assert!(p.x >= 0.0 && p.x + p.w <= size as f32, "{size}px: за холстом {p:?}");
            }
            assert!(bars[0].h > bars[1].h && bars[1].h > bars[2].h, "{size}px: голос убывает {bars:?}");
            assert!(caret.h > bars[0].h, "{size}px: курсор выше голоса");
            for w in all.windows(2) {
                assert!(w[1].x >= w[0].x + w[0].w + 1.0, "{size}px: просвет хотя бы 1 px {w:?}");
            }
        }
    }

    #[test]
    fn glyph_has_a_solid_caret_and_clear_corners() {
        for size in [16, 20, 24, 32, 48] {
            let img = render(IconState::Ready, size, true);
            assert_eq!(img.len(), (size * size * 4) as usize);
            let (x, y) = caret_mid(size, size as f32 * 0.875);
            assert_eq!(px(&img, size, x, y)[3], 255, "курсор, {size}px");
            assert_eq!(px(&img, size, 0, 0)[3], 0, "угол прозрачный, {size}px");
        }
    }

    #[test]
    fn theme_changes_glyph_color() {
        let (x, y) = caret_mid(32, 28.0);
        assert_eq!(px(&render(IconState::Ready, 32, true), 32, x, y)[0], 0xFF);
        assert_eq!(px(&render(IconState::Ready, 32, false), 32, x, y)[0], 0x1C);
    }

    #[test]
    fn recording_paints_the_voice_not_the_caret() {
        let img = render(IconState::Recording, 32, true);
        let (bars, _) = layout(32, 28.0);
        let b = bars[0];
        assert_eq!(px(&img, 32, (b.x + b.w / 2.0) as u32, 16), [RED.0, RED.1, RED.2, 255]);
        let (x, y) = caret_mid(32, 28.0);
        assert_eq!(px(&img, 32, x, y), [0xFF, 0xFF, 0xFF, 255]);
    }

    #[test]
    fn states_differ() {
        let ready = render(IconState::Ready, 32, true);
        for st in [IconState::Paused, IconState::Recording, IconState::Transcribing, IconState::Error] {
            assert_ne!(render(st, 32, true), ready, "{st:?}");
        }
    }

    #[test]
    fn app_icon_is_a_coral_plate_with_the_mark() {
        for size in [16, 24, 32, 48, 256] {
            let img = render_app(size);
            assert!(px(&img, size, 0, 0)[3] < 128, "угол плитки скруглён, {size}px");
            let edge = px(&img, size, size / 2, (size as f32 * 0.06) as u32 + 1);
            assert_eq!(edge, [CORAL.0, CORAL.1, CORAL.2, 255], "коралл у верхнего края, {size}px");
            let (_, bars, c) = app_layout(size);
            let mid = |p: Pill| ((p.x + p.w / 2.0) as u32, size / 2);
            let (x, y) = mid(c);
            assert_eq!(px(&img, size, x, y), [INK.0, INK.1, INK.2, 255], "тёмный курсор, {size}px");
            let (x, y) = mid(bars[0]);
            assert_eq!(px(&img, size, x, y), [0xFF, 0xFF, 0xFF, 255], "белый голос, {size}px");
        }
    }
}
