//! Живая проверка оверлея записи (`src/overlay.rs`): запись → распознаю → ошибка → вспышка → вспышка поверх
//! записи → скрыто.
//!
//! ```text
//! cargo run --example overlay_demo -- [--dark | --light] [--top] [--shots <папка>]
//! ```
//! Громкость голоса имитирует поток: слоги ~4 в секунду, фразы с паузами; кладёт в `LEVEL` так же, как колбэк
//! микрофона (`fetch_max` по битам). Проверки — в выводе, код выхода 1, если какая-то не прошла:
//! фокус не ушёл (окно на переднем плане то же, что до показа), мышь проходит сквозь плашку (`WindowFromPoint`),
//! стили окна, плашка по центру рабочей области, волна движется, после скрытия процесс не тратит CPU.
//! `--shots` — снимки только области вокруг плашки (не всего экрана) в PNG.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{FILETIME, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, MonitorFromWindow,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY, SRCCOPY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetProcessTimes};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    WindowFromPoint, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
};

use verba::audio::recorder::LEVEL;
use verba::overlay::{Overlay, Position, WINDOW_CLASS};

struct Run {
    fg0: HWND,
    shots: Option<PathBuf>,
    failed: bool,
}

impl Run {
    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "ок " } else { "НЕТ" });
        self.failed |= !ok;
    }

    /// Окно на переднем плане то же, что до показа, и это не плашка.
    fn focus(&mut self, when: &str) {
        let fg = unsafe { GetForegroundWindow() };
        let ov = overlay_hwnd();
        self.check(fg == self.fg0 && Some(fg) != ov, &format!("{when}: фокус на месте ({:?})", fg.0));
    }

    fn visible(&mut self, want: bool, when: &str) {
        let v = overlay_hwnd().is_some_and(|h| unsafe { IsWindowVisible(h).as_bool() });
        self.check(v == want, &format!("{when}: плашка {}", if want { "видна" } else { "скрыта" }));
    }

    fn shot(&mut self, name: &str) -> Option<Shot> {
        let h = overlay_hwnd()?;
        let s = capture(h)?;
        if let Some(dir) = &self.shots {
            let path = dir.join(format!("{name}.png"));
            match save_png(&path, &s.rgba, s.w, s.h) {
                Ok(()) => println!("    снимок {}", path.display()),
                Err(e) => println!("    снимок не записан: {e}"),
            }
        }
        Some(s)
    }
}

/// поля снимка вокруг плашки
const PAD: i32 = 24;

struct Shot {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

/// Сколько пикселей внутри плашки (без полей снимка) заметно отличаются. Фон под полупрозрачной плашкой
/// (анимация окна за ней) просвечивает на 8 % — это десятки уровней; столбик против плашки — сотни.
fn changed(a: &Option<Shot>, b: &Option<Shot>) -> Option<usize> {
    let (a, b) = (a.as_ref()?, b.as_ref()?);
    if (a.w, a.h) != (b.w, b.h) {
        return Some(usize::MAX);
    }
    let mut n = 0;
    for y in PAD as u32..a.h.saturating_sub(PAD as u32) {
        for x in PAD as u32..a.w.saturating_sub(PAD as u32) {
            let i = ((y * a.w + x) * 4) as usize;
            let d = (0..3).map(|c| a.rgba[i + c].abs_diff(b.rgba[i + c])).max().unwrap_or(0);
            if d > 60 {
                n += 1;
            }
        }
    }
    Some(n)
}

/// Окно оверлея этого процесса (у другой запущенной Verba — своё, его не трогаем).
fn overlay_hwnd() -> Option<HWND> {
    let me = unsafe { GetCurrentProcessId() };
    let mut after: Option<HWND> = None;
    loop {
        let h = unsafe { FindWindowExW(None, after, WINDOW_CLASS, PCWSTR::null()) }.ok()?;
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        if pid == me {
            return Some(h);
        }
        after = Some(h);
    }
}

/// Область экрана вокруг плашки (с полями) → RGBA. `CAPTUREBLT` — вместе с layered-окнами.
fn capture(h: HWND) -> Option<Shot> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) }.ok()?;
    let (x, y, w, hh) = (r.left - PAD, r.top - PAD, r.right - r.left + 2 * PAD, r.bottom - r.top + 2 * PAD);
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -hh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let out = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok().and_then(|bmp| {
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            let ok = BitBlt(mem, 0, 0, w, hh, Some(screen), x, y, SRCCOPY | CAPTUREBLT).is_ok();
            let src = std::slice::from_raw_parts(bits as *const u8, (w * hh * 4) as usize);
            let rgba: Vec<u8> = src.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], 255]).collect();
            SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            ok.then_some(Shot { rgba, w: w as u32, h: hh as u32 })
        });
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        out
    }
}

fn save_png(path: &Path, rgba: &[u8], w: u32, h: u32) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(rgba).map_err(|e| e.to_string())
}

/// Голос: фразы по 1,6 с с паузой 0,5 с, слоги ~4,3 в секунду, громкость слогов дрожит; фон ≈ −56 дБ.
fn voice(stop: Arc<AtomicBool>) {
    let t0 = Instant::now();
    let mut seed: u32 = 0x1234_5678;
    while !stop.load(Ordering::Relaxed) {
        let t = t0.elapsed().as_secs_f32();
        let phrase = (t % 2.1) < 1.6;
        let syllable = (t * 4.3 * std::f32::consts::TAU).sin().max(0.0).powf(0.6);
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let jitter = 0.4 + 1.0 * (seed >> 8) as f32 / (1u32 << 24) as f32;
        let rms = 0.0015 + if phrase { 0.12 * syllable * jitter } else { 0.0 };
        LEVEL.fetch_max(rms.to_bits(), Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn cpu_ms() -> u64 {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    (t(k) + t(u)) / 10_000
}

fn sleep(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--dark") {
        std::env::set_var("VERBA_OVERLAY_THEME", "dark");
    } else if args.iter().any(|a| a == "--light") {
        std::env::set_var("VERBA_OVERLAY_THEME", "light");
    }
    let position = if args.iter().any(|a| a == "--top") { Position::Top } else { Position::Bottom };
    let shots = args.iter().position(|a| a == "--shots").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    if let Some(d) = &shots {
        let _ = std::fs::create_dir_all(d);
    }
    let mut run = Run { fg0: unsafe { GetForegroundWindow() }, shots, failed: false };
    println!("окно на переднем плане до показа: {:?}", run.fg0.0);

    let ov = match Overlay::spawn() {
        Ok(o) => o,
        Err(e) => {
            println!("НЕТ оверлей не запустился: {e}");
            std::process::exit(1);
        }
    };
    ov.set_position(position);
    let stop = Arc::new(AtomicBool::new(false));
    let v = {
        let s = stop.clone();
        std::thread::spawn(move || voice(s))
    };

    // 1. запись
    let t = Instant::now();
    let c0 = cpu_ms();
    ov.recording();
    sleep(1500);
    println!("    запись: CPU процесса за 1,5 с — {} мс (вместе с потоком «голоса» демо)", cpu_ms() - c0);
    run.visible(true, "запись");
    run.focus("запись");
    if let Some(h) = overlay_hwnd() {
        let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
        let need = [
            (WS_EX_NOACTIVATE.0, "NOACTIVATE"),
            (WS_EX_TRANSPARENT.0, "TRANSPARENT"),
            (WS_EX_LAYERED.0, "LAYERED"),
            (WS_EX_TOPMOST.0, "TOPMOST"),
            (WS_EX_TOOLWINDOW.0, "TOOLWINDOW"),
        ];
        let missing: Vec<&str> = need.iter().filter(|(b, _)| ex & b == 0).map(|(_, n)| *n).collect();
        run.check(missing.is_empty(), &format!("стили окна 0x{ex:08X}, нет: {missing:?}"));
        let mut r = RECT::default();
        let _ = unsafe { GetWindowRect(h, &mut r) };
        let center = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
        let under = unsafe { WindowFromPoint(center) };
        run.check(under != h, &format!("мышь сквозь плашку: под её серединой окно {:?}", under.0));
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = unsafe { MonitorFromWindow(run.fg0, MONITOR_DEFAULTTOPRIMARY) };
        if unsafe { GetMonitorInfoW(mon, &mut mi) }.as_bool() {
            let wk = mi.rcWork;
            let (gl, gr) = (r.left - wk.left, wk.right - r.right);
            println!(
                "    плашка {}×{} в ({}, {}); рабочая область ({}, {})–({}, {})",
                r.right - r.left,
                r.bottom - r.top,
                r.left,
                r.top,
                wk.left,
                wk.top,
                wk.right,
                wk.bottom
            );
            run.check((gl - gr).abs() <= 1, &format!("по центру: слева {gl}, справа {gr}"));
            match position {
                Position::Top => run.check(r.top > wk.top, &format!("у верхнего края: {} px", r.top - wk.top)),
                _ => run.check(
                    r.bottom < wk.bottom,
                    &format!("над панелью задач: до низа рабочей области {} px", wk.bottom - r.bottom),
                ),
            }
        }
    }
    let a = run.shot("1-zapis-a");
    sleep(210);
    let b = run.shot("1-zapis-b");
    let n = changed(&a, &b);
    run.check(n.is_some_and(|n| n > 20), &format!("волна движется: за 210 мс изменилось пикселей плашки — {n:?}"));

    // 2. распознаю
    sleep(3600u64.saturating_sub(t.elapsed().as_millis() as u64));
    ov.transcribing();
    sleep(400);
    run.visible(true, "распознаю");
    run.focus("распознаю");
    let c = run.shot("2-raspoznayu");
    sleep(300);
    let d = run.shot("2-raspoznayu-b");
    let n = changed(&c, &d);
    run.check(n == Some(0), &format!("распознаю: волна замерла — за 300 мс изменилось пикселей плашки {n:?}"));

    // 3. ошибка — 5 с и уходит
    ov.error(
        "Сервис не ответил за 7 с → проверь сеть или прокси; запись сохранена — «вставить последнее» отправит её снова",
    );
    sleep(500);
    run.visible(true, "ошибка");
    run.focus("ошибка");
    run.shot("3-oshibka");
    sleep(5000);
    run.visible(false, "ошибка через 5,5 с");

    // 4. вспышка
    ov.flash("Пауза");
    sleep(400);
    run.visible(true, "вспышка «Пауза»");
    run.shot("4-pauza");
    sleep(1100);
    run.visible(false, "вспышка через 1,5 с");

    // 5. вспышка поверх записи — запись возвращается
    ov.recording();
    sleep(700);
    ov.flash("Слушаю");
    sleep(300);
    run.shot("5-slushayu");
    sleep(1300);
    run.visible(true, "после вспышки поверх записи");
    run.shot("6-zapis-snova");
    run.focus("после вспышки");

    // 6. скрыто, простой
    ov.hide();
    stop.store(true, Ordering::Relaxed);
    let _ = v.join();
    sleep(400);
    run.visible(false, "скрыто");
    run.focus("скрыто");
    let c0 = cpu_ms();
    sleep(2000);
    let spent = cpu_ms() - c0;
    run.check(spent <= 16, &format!("простой: CPU процесса за 2 с — {spent} мс"));

    if run.failed {
        println!("ИТОГ: есть непрошедшие проверки");
        std::process::exit(1);
    }
    println!("ИТОГ: все проверки прошли");
}
