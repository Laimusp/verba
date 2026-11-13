//! Окно настроек на Slint (`ui/settings.slint`). Живёт в главном потоке: цикл Slint — он же цикл
//! сообщений трея. Создаётся по требованию, при закрытии (крестик) освобождается — Verba остаётся в
//! трее; замер: с окном ~5,5 МБ private, после закрытия ~3,5 МБ, открытие 10–35 мс (`examples/slint_probe.rs`).
//!
//! Данные идут в одну сторону: контроллер собирает `ViewData` и шлёт `push(Msg::View)` из своего
//! потока → окно перерисовывается целиком. Действия окна → `UiEvent::Settings(Cmd)` → контроллер.
//! Поток UI не ждёт ни сети, ни диска, ни Credential Manager (CLAUDE.md §2): всё это — у контроллера.

mod ui {
    slint::include_modules!();
}

use std::cell::RefCell;
use std::rc::Rc;

use crossbeam_channel::Sender;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::config::{Engine, HotkeyMode};
use crate::hotkey::hook::HotkeyId;
use crate::overlay::Position;
use crate::tray::UiEvent;

/// Порядок строк хоткеев в окне.
pub const HOTKEY_ROWS: [HotkeyId; 4] = [HotkeyId::Dictate, HotkeyId::Pause, HotkeyId::PasteLast, HotkeyId::Cancel];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineState {
    #[default]
    None,
    Busy,
    Ok,
    Err,
}

/// Строка состояния под настройкой: точка цвета состояния и слова.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub state: LineState,
    pub text: String,
}

impl Line {
    pub fn busy(t: impl Into<String>) -> Self {
        Self { state: LineState::Busy, text: t.into() }
    }
    pub fn ok(t: impl Into<String>) -> Self {
        Self { state: LineState::Ok, text: t.into() }
    }
    pub fn err(t: impl Into<String>) -> Self {
        Self { state: LineState::Err, text: t.into() }
    }
    pub fn is_busy(&self) -> bool {
        self.state == LineState::Busy
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelItem {
    pub id: String,
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotkeyItem {
    pub title: String,
    pub hint: String,
    pub label: String,
    pub capturing: bool,
    pub warn: String,
}

/// Всё, что показывает окно. Собирает контроллер; `Send` — уходит в поток UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewData {
    pub paused: bool,
    pub dictate_label: String,
    pub codex: bool,
    /// «sk-or-v1-…abcd»; пусто — ключа нет
    pub key_mask: String,
    pub key_line: Line,
    pub models: Vec<ModelItem>,
    /// модели OpenRouter, которых ещё нет в своём списке
    pub catalog: Vec<String>,
    pub catalog_line: Line,
    /// «SOCKS5 1.2.3.4:1080 · с логином»; пусто — напрямую
    pub proxy: String,
    pub proxy_line: Line,
    /// проверка не прошла — можно сохранить всё равно
    pub proxy_unsaved: bool,
    pub codex_login: Line,
    pub codex_line: Line,
    pub hotkeys: Vec<HotkeyItem>,
    pub mode: HotkeyMode,
    pub hold_ms: u64,
    pub remember_field: bool,
    pub trailing_space: bool,
    pub humanize: bool,
    pub restore_clipboard: bool,
    pub games: Vec<String>,
    /// первый пункт — «как в Windows»
    pub devices: Vec<String>,
    pub device: usize,
    pub keep_open: bool,
    pub snd_rec: bool,
    pub snd_pause: bool,
    pub snd_err: bool,
    pub overlay: Position,
    pub autostart: bool,
    pub autostart_busy: bool,
    pub autostart_line: Line,
    pub restart_elevated: bool,
    pub version: String,
}

pub enum Msg {
    View(Box<ViewData>),
    /// ключ сохранён — очистить поле
    KeySaved,
    ProxySaved,
    ModelAdded,
    GameAdded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    RememberField,
    TrailingSpace,
    Humanize,
    RestoreClipboard,
    KeepOpen,
    SoundRecording,
    SoundPause,
    SoundErrors,
}

/// Действие из окна настроек.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Opened,
    Closed,
    Engine(Engine),
    KeySave(String),
    KeyCheck,
    KeyRemove,
    ModelsRefresh,
    ModelActive(String),
    ModelAdd(String),
    ModelRemove(String),
    /// пусто — проверить сохранённый прокси
    ProxyCheck(String),
    ProxySaveAnyway,
    ProxyRemove,
    CodexCheck,
    Capture(HotkeyId),
    CaptureCancel,
    Mode(HotkeyMode),
    HoldMs(u64),
    Flag(Flag, bool),
    GameAdd(String),
    GameRemove(String),
    /// номер в списке устройств окна (0 — как в Windows)
    Device(usize),
    Overlay(Position),
    Autostart(bool),
    Restart,
    OpenLog,
    OpenConfig,
    Shortcut,
    OpenUrl(String),
    TogglePause,
    Exit,
}

/// `act(имя, аргумент)` из .slint → действие. Незнакомое — None.
pub fn parse_act(name: &str, arg: &str) -> Option<Cmd> {
    let num = || arg.trim().parse::<i64>().ok();
    let text = || arg.trim().to_string();
    Some(match name {
        "engine" => Cmd::Engine(if arg == "codex" { Engine::Codex } else { Engine::OpenRouter }),
        "key-save" if !arg.trim().is_empty() => Cmd::KeySave(text()),
        "key-check" => Cmd::KeyCheck,
        "key-remove" => Cmd::KeyRemove,
        "models-refresh" => Cmd::ModelsRefresh,
        "model-active" => Cmd::ModelActive(text()),
        "model-add" if !arg.trim().is_empty() => Cmd::ModelAdd(text()),
        "model-remove" => Cmd::ModelRemove(text()),
        "proxy-check" => Cmd::ProxyCheck(text()),
        "proxy-save-anyway" => Cmd::ProxySaveAnyway,
        "proxy-remove" => Cmd::ProxyRemove,
        "codex-check" => Cmd::CodexCheck,
        "capture" => Cmd::Capture(*HOTKEY_ROWS.get(usize::try_from(num()?).ok()?)?),
        "capture-cancel" => Cmd::CaptureCancel,
        "mode" => Cmd::Mode(match num()? {
            0 => HotkeyMode::Hold,
            1 => HotkeyMode::Toggle,
            2 => HotkeyMode::Hybrid,
            _ => return None,
        }),
        "hold-ms" => Cmd::HoldMs(num()?.clamp(100, 2000) as u64),
        "flag" => {
            let (k, v) = arg.split_once('=')?;
            let flag = match k {
                "remember_field" => Flag::RememberField,
                "trailing_space" => Flag::TrailingSpace,
                "humanize" => Flag::Humanize,
                "restore_clipboard" => Flag::RestoreClipboard,
                "keep_open" => Flag::KeepOpen,
                "snd_rec" => Flag::SoundRecording,
                "snd_pause" => Flag::SoundPause,
                "snd_err" => Flag::SoundErrors,
                _ => return None,
            };
            Cmd::Flag(flag, v == "1")
        }
        "game-add" if !arg.trim().is_empty() => Cmd::GameAdd(text()),
        "game-remove" => Cmd::GameRemove(text()),
        "device" => Cmd::Device(usize::try_from(num()?).ok()?),
        "overlay" => Cmd::Overlay(match num()? {
            0 => Position::Bottom,
            1 => Position::Top,
            2 => Position::Off,
            _ => return None,
        }),
        "autostart" => Cmd::Autostart(arg == "1"),
        "restart" => Cmd::Restart,
        "open-log" => Cmd::OpenLog,
        "open-config" => Cmd::OpenConfig,
        "shortcut" => Cmd::Shortcut,
        "url" if arg.starts_with("https://") => Cmd::OpenUrl(text()),
        "pause" => Cmd::TogglePause,
        "exit" => Cmd::Exit,
        _ => return None,
    })
}

/// Маска ключа для окна: «sk-or-v1-…abcd» (начало до последнего «-» и 4 последних знака).
pub fn mask_key(key: &str) -> String {
    let key = key.trim();
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 12 {
        return "сохранён".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    let head = match key.rfind('-') {
        Some(i) if i < 12 => &key[..=i],
        _ => &key[..3.min(key.len())],
    };
    format!("{head}…{tail}")
}

// ---------- поток UI ----------

#[derive(Default)]
struct State {
    tx: Option<Sender<UiEvent>>,
    win: Option<ui::SettingsWindow>,
    last: Option<Box<ViewData>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Канал к контроллеру. Звать в главном потоке до цикла Slint.
pub fn init(tx: Sender<UiEvent>) {
    STATE.with(|s| s.borrow_mut().tx = Some(tx));
}

/// Открыть окно (или поднять открытое). Из любого потока, когда цикл Slint уже идёт.
pub fn request_open() {
    if let Err(e) = slint::invoke_from_event_loop(open) {
        crate::warn!("окно настроек: {e}");
    }
}

/// Открыть окно на первом круге цикла Slint (до запуска цикла `invoke_from_event_loop` отказывает).
pub fn open_at_start() {
    slint::Timer::single_shot(std::time::Duration::ZERO, open);
}

/// Данные для окна. Из любого потока; окно закрыто — запоминается для следующего открытия.
pub fn push(msg: Msg) {
    let _ = slint::invoke_from_event_loop(move || apply(msg));
}

fn send(cmd: Cmd) {
    STATE.with(|s| {
        if let Some(tx) = &s.borrow().tx {
            let _ = tx.send(UiEvent::Settings(cmd));
        }
    });
}

fn logo() -> slint::Image {
    let size = 64;
    let rgba = crate::icons::render_app(size);
    slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, size, size))
}

/// Моноширинный шрифт окна (сочетания клавиш, ключ, прокси, имена моделей): Cascadia Mono есть в Windows 11, нет —
/// Consolas (Windows 10). Slint запасных имён не принимает, поэтому выбор здесь; проверка — таблица шрифтов GDI в
/// памяти, без диска, раз на процесс.
fn mono_family() -> &'static str {
    static MONO: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    MONO.get_or_init(|| if crate::overlay::has_font("Cascadia Mono") { "Cascadia Mono" } else { "Consolas" })
}

fn open() {
    let existing = STATE.with(|s| s.borrow().win.as_ref().map(|w| w.as_weak()));
    if let Some(w) = existing.and_then(|w| w.upgrade()) {
        let _ = w.show();
        to_front(&w);
        return;
    }
    let started = std::time::Instant::now();
    let win = match ui::SettingsWindow::new() {
        Ok(w) => w,
        Err(e) => {
            crate::error!("окно настроек не создано: {e}");
            return;
        }
    };
    win.set_logo(logo());
    win.global::<ui::Theme>().set_font_mono(mono_family().into());
    win.on_act(|name, arg| match parse_act(&name, &arg) {
        Some(cmd) => send(cmd),
        None => crate::warn!("окно настроек: незнакомое действие «{name}»"),
    });
    win.window().on_close_requested(|| {
        // окно освобождается после выхода из обработчика: Slint ещё держит его
        let _ = slint::invoke_from_event_loop(close);
        slint::CloseRequestResponse::HideWindow
    });
    if let Some(v) = STATE.with(|s| s.borrow().last.clone()) {
        fill(&win, &v);
    }
    if let Err(e) = win.show() {
        crate::error!("окно настроек не показано: {e}");
        return;
    }
    to_front(&win);
    STATE.with(|s| s.borrow_mut().win = Some(win));
    crate::info!("окно настроек открыто за {} мс", started.elapsed().as_millis());
    send(Cmd::Opened);
}

fn close() {
    let win = STATE.with(|s| s.borrow_mut().win.take());
    if let Some(w) = win {
        let _ = w.hide();
        drop(w);
        send(Cmd::Closed);
    }
}

/// Закрыть окно, если открыто (выход из Verba).
pub fn close_if_open() {
    close();
}

/// HWND окна. Slint создаёт системное окно на следующем круге цикла после `show()` — до этого None.
fn hwnd_of(win: &ui::SettingsWindow) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = win.window().window_handle();
    let h = handle.window_handle().ok()?;
    match h.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
        _ => None,
    }
}

/// Окно вперёд и знак Verba в заголовке. Окно ещё не создано — повтор через 20 мс (до 25 раз).
/// Вперёд — в отдельном потоке: приём с пустым вводом мыши ждёт до ~180 мс
/// (`target::win::bring_to_front`); открытое из трея или вторым запуском окно иначе встанет под текущим.
fn to_front(win: &ui::SettingsWindow) {
    fn attempt(weak: slint::Weak<ui::SettingsWindow>, left: u32) {
        let Some(win) = weak.upgrade() else { return };
        match hwnd_of(&win) {
            Some(hwnd) => {
                set_icon(hwnd);
                let _ = std::thread::Builder::new().name("verba-front".into()).spawn(move || {
                    let _ = crate::target::win::bring_to_front(hwnd);
                });
            }
            None if left > 0 => {
                slint::Timer::single_shot(std::time::Duration::from_millis(20), move || attempt(weak, left - 1));
            }
            None => {}
        }
    }
    attempt(win.as_weak(), 25);
}

/// Знак Verba в заголовке окна и на панели задач — иконка из ресурса exe (build.rs, номер 1).
fn set_icon(hwnd: isize) {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, SM_CXICON, SM_CXSMICON,
        WM_SETICON,
    };
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else { return };
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let size = GetSystemMetrics(metric);
            // LR_SHARED: иконку держит система, освобождать её не нужно
            if let Ok(icon) = LoadImageW(Some(module.into()), PCWSTR(std::ptr::without_provenance::<u16>(1)) /* MAKEINTRESOURCE(1): номер, не адрес */, IMAGE_ICON, size, size, LR_SHARED) {
                SendMessageW(HWND(hwnd as *mut _), WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}

fn apply(msg: Msg) {
    let win = STATE.with(|s| s.borrow().win.as_ref().map(|w| w.as_weak()));
    let win = win.and_then(|w| w.upgrade());
    match msg {
        Msg::View(v) => {
            if let Some(w) = &win {
                fill(w, &v);
            }
            STATE.with(|s| s.borrow_mut().last = Some(v));
        }
        Msg::KeySaved => {
            if let Some(w) = &win {
                w.invoke_key_saved();
            }
        }
        Msg::ProxySaved => {
            if let Some(w) = &win {
                w.invoke_proxy_saved();
            }
        }
        Msg::ModelAdded => {
            if let Some(w) = &win {
                w.invoke_model_added();
            }
        }
        Msg::GameAdded => {
            if let Some(w) = &win {
                w.invoke_game_added();
            }
        }
    }
}

fn line(l: &Line) -> ui::Line {
    let state = match l.state {
        LineState::None => 0,
        LineState::Busy => 1,
        LineState::Ok => 2,
        LineState::Err => 3,
    };
    ui::Line { state, text: SharedString::from(l.text.as_str()) }
}

fn strings(v: &[String]) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(v.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>())))
}

fn fill(w: &ui::SettingsWindow, v: &ViewData) {
    let models: Vec<ui::ModelRow> = v
        .models
        .iter()
        .map(|m| ui::ModelRow { id: m.id.as_str().into(), name: m.name.as_str().into(), active: m.active })
        .collect();
    let hotkeys: Vec<ui::HotkeyRow> = v
        .hotkeys
        .iter()
        .map(|h| ui::HotkeyRow {
            title: h.title.as_str().into(),
            hint: h.hint.as_str().into(),
            label: h.label.as_str().into(),
            capturing: h.capturing,
            warn: h.warn.as_str().into(),
        })
        .collect();
    let mode = match v.mode {
        HotkeyMode::Hold => 0,
        HotkeyMode::Toggle => 1,
        HotkeyMode::Hybrid => 2,
    };
    let overlay = match v.overlay {
        Position::Bottom => 0,
        Position::Top => 1,
        Position::Off => 2,
    };
    w.set_view(ui::View {
        paused: v.paused,
        dictate_label: v.dictate_label.as_str().into(),
        codex: v.codex,
        key_mask: v.key_mask.as_str().into(),
        key_line: line(&v.key_line),
        models: ModelRc::from(Rc::new(VecModel::from(models))),
        catalog: strings(&v.catalog),
        catalog_line: line(&v.catalog_line),
        proxy: v.proxy.as_str().into(),
        proxy_line: line(&v.proxy_line),
        proxy_unsaved: v.proxy_unsaved,
        codex_login: line(&v.codex_login),
        codex_line: line(&v.codex_line),
        hotkeys: ModelRc::from(Rc::new(VecModel::from(hotkeys))),
        mode,
        hold_ms: v.hold_ms.min(i32::MAX as u64) as i32,
        remember_field: v.remember_field,
        trailing_space: v.trailing_space,
        humanize: v.humanize,
        restore_clipboard: v.restore_clipboard,
        games: strings(&v.games),
        devices: strings(&v.devices),
        device: v.device.min(i32::MAX as usize) as i32,
        keep_open: v.keep_open,
        snd_rec: v.snd_rec,
        snd_pause: v.snd_pause,
        snd_err: v.snd_err,
        overlay,
        autostart: v.autostart,
        autostart_busy: v.autostart_busy,
        autostart_line: line(&v.autostart_line),
        restart_elevated: v.restart_elevated,
        version: v.version.as_str().into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acts_parse() {
        assert_eq!(parse_act("engine", "codex"), Some(Cmd::Engine(Engine::Codex)));
        assert_eq!(parse_act("key-save", "  sk-or-v1-abc  "), Some(Cmd::KeySave("sk-or-v1-abc".into())));
        assert_eq!(parse_act("key-save", "   "), None, "пустой ключ не уходит");
        assert_eq!(parse_act("capture", "1"), Some(Cmd::Capture(HotkeyId::Pause)));
        assert_eq!(parse_act("capture", "9"), None);
        assert_eq!(parse_act("mode", "2"), Some(Cmd::Mode(HotkeyMode::Hybrid)));
        assert_eq!(parse_act("hold-ms", "50"), Some(Cmd::HoldMs(100)), "порог не ниже 100 мс");
        assert_eq!(parse_act("hold-ms", "5000"), Some(Cmd::HoldMs(2000)));
        assert_eq!(parse_act("flag", "keep_open=1"), Some(Cmd::Flag(Flag::KeepOpen, true)));
        assert_eq!(parse_act("flag", "humanize=1"), Some(Cmd::Flag(Flag::Humanize, true)));
        assert_eq!(parse_act("flag", "humanize=0"), Some(Cmd::Flag(Flag::Humanize, false)));
        assert_eq!(parse_act("flag", "nope=1"), None);
        assert_eq!(parse_act("overlay", "2"), Some(Cmd::Overlay(Position::Off)));
        assert_eq!(parse_act("url", "file:///c:/x"), None, "наружу — только https");
        assert_eq!(parse_act("что-то", ""), None);
    }

    #[test]
    fn key_mask_keeps_prefix_and_tail() {
        // ключ собирается в тесте: строка вида настоящего ключа в коде — повод для проверки выгрузки
        let fake = format!("{}{}", "sk-or-v1-", "x".repeat(28) + "cdef");
        assert_eq!(mask_key(&fake), "sk-or-v1-…cdef");
        assert_eq!(mask_key("abcdefghijklmnopqrstuvwxyz"), "abc…wxyz");
        assert_eq!(mask_key("short"), "сохранён");
    }
}
