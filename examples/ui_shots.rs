//! Снимки окна настроек (T-006) — без окна на экране: своя платформа Slint рисует программно в память тем же
//! отрисовщиком и теми же шрифтами системы, что и окно Verba.
//!
//! ```text
//! cargo run --example ui_shots -- <папка> [--scale 1.5]
//! ```
//! В папке: `<тема>-<страница>.png` — тема dark/light, страницы «Распознавание», «Хоткеи», «Вставка», «Общее».
//! Данные окна — выдуманные (ключ, прокси, модели), не из config.json.

mod ui {
    slint::include_modules!();
}

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, ModelRc, PlatformError, SharedString, VecModel};

const PAGES: [(i32, &str); 4] = [(0, "recognition"), (1, "hotkeys"), (2, "paste"), (4, "general")];

thread_local! {
    /// окно, которое платформа отдала последнему созданному компоненту
    static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct Headless;

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let w = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        LAST.with(|l| *l.borrow_mut() = Some(w.clone()));
        Ok(w)
    }
}

fn strings(v: &[&str]) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(v.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>())))
}

fn line(state: i32, text: &str) -> ui::Line {
    ui::Line { state, text: text.into() }
}

/// Окно в том виде, в каком его видит владелец: ключ есть, две модели, прокси проверен.
fn view() -> ui::View {
    let models = vec![
        ui::ModelRow { id: "openai/gpt-4o-mini-transcribe".into(), name: "GPT-4o mini Transcribe".into(), active: true },
        ui::ModelRow { id: "openai/whisper-large-v3-turbo".into(), name: "Whisper Large v3 Turbo".into(), active: false },
    ];
    let hk = |title: &str, hint: &str, label: &str| ui::HotkeyRow {
        title: title.into(),
        hint: hint.into(),
        label: label.into(),
        capturing: false,
        warn: SharedString::new(),
    };
    let hotkeys = vec![
        hk("Диктовка", "Держи и говори", "Ctrl + Space"),
        hk("Пауза", "Хоткей диктовки уходит в приложение", "Ctrl + Alt + P"),
        hk("Вставить последнее", "Ещё раз — или повтор отправки", "Ctrl + Alt + V"),
        hk("Отмена", "Пока идёт запись", "Esc"),
    ];
    ui::View {
        dictate_label: "Ctrl + Space — держи и говори".into(),
        key_mask: "sk-or-v1-…cdef".into(),
        key_line: line(2, "Ключ принят · баланс $4,12"),
        models: ModelRc::from(Rc::new(VecModel::from(models))),
        catalog: strings(&["openai/whisper-1"]),
        proxy: "SOCKS5 1.2.3.4:1080 · с логином".into(),
        proxy_line: line(2, "Прокси отвечает: 180 мс"),
        hotkeys: ModelRc::from(Rc::new(VecModel::from(hotkeys))),
        hold_ms: 300,
        remember_field: true,
        trailing_space: true,
        restore_clipboard: true,
        games: strings(&["cs2.exe"]),
        devices: strings(&["Как в Windows", "Микрофон (USB Audio)"]),
        keep_open: false,
        snd_pause: true,
        snd_err: true,
        autostart: true,
        version: env!("CARGO_PKG_VERSION").into(),
        ..Default::default()
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

fn shot(dir: &Path, scale: f32, dark: bool, page: i32, page_name: &str) -> Result<PathBuf, String> {
    let win = ui::SettingsWindow::new().map_err(|e| e.to_string())?;
    let mw = LAST.with(|l| l.borrow().clone()).ok_or("платформа не отдала окно")?;
    mw.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
    let (w, h) = ((760.0 * scale).round() as u32, (580.0 * scale).round() as u32);
    win.window().set_size(slint::PhysicalSize::new(w, h));
    win.global::<ui::Theme>().set_scheme(if dark { 1 } else { 2 });
    let size = 64;
    let logo = verba::icons::render_app(size);
    win.set_logo(slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(&logo, size, size)));
    win.set_view(view());
    win.invoke_show_page(page);
    win.show().map_err(|e| e.to_string())?;
    slint::platform::update_timers_and_animations();
    let snap = win.window().take_snapshot().map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}-{page_name}.png", if dark { "dark" } else { "light" }));
    save_png(&path, snap.as_bytes(), snap.width(), snap.height())?;
    win.hide().map_err(|e| e.to_string())?;
    Ok(path)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut scale = 1.0f32;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--scale" => scale = args.next().and_then(|v| v.parse().ok()).unwrap_or(1.0),
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let Some(dir) = dir else {
        eprintln!("cargo run --example ui_shots -- <папка> [--scale 1.5]");
        std::process::exit(2);
    };
    std::fs::create_dir_all(&dir).expect("папка снимков");
    slint::platform::set_platform(Box::new(Headless)).expect("платформа Slint");
    let mut failed = false;
    for dark in [true, false] {
        for (page, name) in PAGES {
            match shot(&dir, scale, dark, page, name) {
                Ok(p) => println!("{}", p.display()),
                Err(e) => {
                    println!("НЕТ {name}: {e}");
                    failed = true;
                }
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
