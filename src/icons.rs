//! Значок трея — рисуется кодом по расстояниям до фигур (сглаженные края при любом DPI).
//! Стиль (CLAUDE.md «Стиль интерфейса»): заливка без обводок, состояние — цветом;
//! цвет глифа — по теме панели задач (белый на тёмной, почти чёрный на светлой).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IconState {
    Ready,
    Paused,
    Recording,
    Transcribing,
    Error,
}

type Rgb = (u8, u8, u8);

const RED: Rgb = (0xE5, 0x48, 0x4D);
const AMBER: Rgb = (0xF5, 0xA5, 0x24);

fn length(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

/// Расстояние до отрезка AB.
fn seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    length(px - ax - t * dx, py - ay - t * dy)
}

/// Глиф микрофона: капсула, дуга-держатель, ножка. Расстояние со знаком (в долях стороны).
fn mic(x: f32, y: f32) -> f32 {
    let capsule = seg(x, y, 0.5, 0.25, 0.5, 0.45) - 0.165;
    let (cx, cy, r, w) = (0.5, 0.45, 0.29, 0.095);
    let arc = if y >= cy {
        (length(x - cx, y - cy) - r).abs() - w / 2.0
    } else {
        // выше центра дуги — только скруглённые концы
        length(x - (cx - r), y - cy).min(length(x - (cx + r), y - cy)) - w / 2.0
    };
    let stem = seg(x, y, 0.5, cy + r, 0.5, 0.93) - w / 2.0;
    capsule.min(arc).min(stem)
}

/// Покрытие пикселя фигурой: 1 внутри, 0 снаружи, сглаженный край в один пиксель.
fn cover(d_unit: f32, size: f32) -> f32 {
    (0.5 - d_unit * size).clamp(0.0, 1.0)
}

/// RGBA, строки сверху вниз, без предумножения альфы.
pub fn render(state: IconState, size: u32, dark_taskbar: bool) -> Vec<u8> {
    let fg: Rgb = if dark_taskbar { (0xFF, 0xFF, 0xFF) } else { (0x1C, 0x1C, 0x1E) };
    let s = size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let (x, y) = ((px as f32 + 0.5) / s, (py as f32 + 0.5) / s);
            let glyph = cover(mic(x, y), s);
            let (color, alpha) = match state {
                IconState::Ready => (fg, glyph),
                IconState::Recording => (RED, glyph),
                IconState::Transcribing => (AMBER, glyph),
                IconState::Paused => {
                    // приглушённый микрофон, перечёркнутый косой чертой с просветом вокруг неё
                    let slash = seg(x, y, 0.16, 0.10, 0.84, 0.90);
                    let line = cover(slash - 0.055, s);
                    let gap = cover(slash - 0.13, s);
                    let dim = glyph * 0.45 * (1.0 - gap);
                    (fg, dim.max(line))
                }
                IconState::Error => {
                    // красная точка в углу, вокруг неё просвет
                    let dot = length(x - 0.78, y - 0.78);
                    let badge = cover(dot - 0.2, s);
                    let gap = cover(dot - 0.29, s);
                    let base = glyph * (1.0 - gap);
                    if badge > 0.0 {
                        (RED, badge.max(base))
                    } else {
                        (fg, base)
                    }
                }
            };
            let i = ((py * size + px) * 4) as usize;
            out[i] = color.0;
            out[i + 1] = color.1;
            out[i + 2] = color.2;
            out[i + 3] = (alpha * 255.0).round() as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(img: &[u8], size: u32, x: u32, y: u32) -> u8 {
        img[((y * size + x) * 4 + 3) as usize]
    }

    #[test]
    fn glyph_is_centered_and_corners_are_clear() {
        for size in [16, 20, 24, 32, 48] {
            let img = render(IconState::Ready, size, true);
            assert_eq!(img.len(), (size * size * 4) as usize);
            assert_eq!(alpha_at(&img, size, size / 2, size / 3), 255, "капсула в центре сверху, {size}px");
            assert_eq!(alpha_at(&img, size, 0, 0), 0, "угол прозрачный, {size}px");
        }
    }

    #[test]
    fn theme_changes_glyph_color() {
        let dark = render(IconState::Ready, 32, true);
        let light = render(IconState::Ready, 32, false);
        let i = ((10 * 32 + 16) * 4) as usize;
        assert_eq!(dark[i], 0xFF);
        assert_eq!(light[i], 0x1C);
    }

    #[test]
    fn states_differ() {
        let ready = render(IconState::Ready, 32, true);
        for st in [IconState::Paused, IconState::Recording, IconState::Transcribing, IconState::Error] {
            assert_ne!(render(st, 32, true), ready, "{st:?}");
        }
    }
}
