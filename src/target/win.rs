//! Win32-часть вставки в запомненное поле: снимок переднего окна и фокуса, классическое поле
//! Edit/RichEdit, перевод окна вперёд мимо foreground lock, пауза во вводе человека.
//!
//! Все вызовы здесь не ждут чужой процесс бесконечно: чтение окна (класс, прямоугольник, поток)
//! сообщений не шлёт, сообщения полю — `SendMessageTimeoutW` с `SMTO_ABORTIFHUNG`, к зависшему окну
//! (`IsHungAppWindow`) не подключаемся.

use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, IsWindowEnabled, SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE,
    KEYBDINPUT, KEYEVENTF_KEYUP, LASTINPUTINFO, MOUSEINPUT, VIRTUAL_KEY, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindow, IsZoomed, PeekMessageW, SendMessageTimeoutW,
    SetForegroundWindow, ShowWindow, ShowWindowAsync, GA_ROOT, GUITHREADINFO, GWL_STYLE, GW_OWNER, MSG, PM_NOREMOVE,
    SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SW_RESTORE, SW_SHOWMINNOACTIVE, WM_GETTEXTLENGTH, WS_CAPTION,
};

use crate::hotkey::hook::{self, INJECTED_MARK, SWALLOWED_MARK};

/// Окно числом: `HWND` — сырой указатель, между потоками его не передать.
pub type Hwnd = isize;

pub fn hwnd(h: Hwnd) -> HWND {
    HWND(h as *mut _)
}

fn raw(h: HWND) -> Hwnd {
    h.0 as isize
}

const EM_GETSEL: u32 = 0x00B0;
const EM_REPLACESEL: u32 = 0x00C2;
const ES_READONLY: u32 = 0x0800;
/// Сообщение полю, которое не ответило за столько, считается зависшим.
const SEND_TIMEOUT_MS: u32 = 1000;

/// Переднее окно и его поле в фокусе.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Window {
    /// верхнее окно (то, что на переднем плане)
    pub top: Hwnd,
    /// окно с фокусом клавиатуры в потоке верхнего окна (`GetGUIThreadInfo`), иначе — окно каретки
    pub focus: Hwnd,
    pub caret: Hwnd,
    pub thread: u32,
    pub pid: u32,
    /// имя exe строчными: `notepad.exe`
    pub exe: String,
    /// класс окна `focus`: `Edit`, `RichEditD2DPT`, `Chrome_WidgetWin_1`…
    pub class: String,
    pub fullscreen: bool,
}

pub fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn exe_name(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or("").to_lowercase()
    }
}

pub fn is_window(h: Hwnd) -> bool {
    h != 0 && unsafe { IsWindow(Some(hwnd(h))) }.as_bool()
}

pub fn foreground_hwnd() -> Hwnd {
    raw(unsafe { GetForegroundWindow() })
}

/// Окно с фокусом в потоке `thread` (`GetGUIThreadInfo`: сообщений не шлёт).
pub fn focus_of(thread: u32) -> (Hwnd, Hwnd) {
    let mut gti = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    if unsafe { GetGUIThreadInfo(thread, &mut gti) }.is_ok() {
        (raw(gti.hwndFocus), raw(gti.hwndCaret))
    } else {
        (0, 0)
    }
}

/// Снимок переднего окна. Только чтение состояния окон: десятки микросекунд, чужой процесс не ждёт.
pub fn foreground() -> Option<Window> {
    let top = unsafe { GetForegroundWindow() };
    if top.is_invalid() {
        return None;
    }
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(top, Some(&mut pid)) };
    if thread == 0 {
        return None;
    }
    let (focus, caret) = focus_of(thread);
    let field = if focus != 0 { focus } else { caret };
    Some(Window {
        top: raw(top),
        focus: field,
        caret,
        thread,
        pid,
        exe: exe_name(pid),
        class: if field != 0 { class_name(hwnd(field)) } else { String::new() },
        fullscreen: is_fullscreen(top),
    })
}

/// Окно закрывает монитор целиком и это не развёрнутое окно с рамкой — полноэкранная игра или
/// видео. Рабочий стол и панель задач тоже «во весь экран», но не в счёт.
pub fn is_fullscreen(top: HWND) -> bool {
    let class = class_name(top);
    if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        return false;
    }
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(top, &mut r).is_err() {
            return false;
        }
        let mon = MonitorFromWindow(top, MONITOR_DEFAULTTONULL);
        if mon.is_invalid() {
            return false;
        }
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return false;
        }
        let style = GetWindowLongW(top, GWL_STYLE) as u32;
        let framed_max = IsZoomed(top).as_bool() && style & WS_CAPTION.0 == WS_CAPTION.0;
        covers(&r, &mi.rcMonitor) && !framed_max
    }
}

pub fn covers(r: &RECT, m: &RECT) -> bool {
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// Классическое поле Windows: `EM_REPLACESEL` вставляет в его каретку без фокуса, а система сама
/// переносит строку в чужой процесс (номер сообщения < `WM_USER`) — так делает AutoHotkey
/// `EditPaste`. WinForms `TextBox`/`RichTextBox` — те же классы под другим именем.
pub fn is_classic_edit(class: &str) -> bool {
    const CLASSES: [&str; 7] = ["Edit", "RichEdit20W", "RichEdit20A", "RICHEDIT50W", "RICHEDIT60W", "RichEditD2D", "RichEditD2DPT"];
    let lower = class.to_ascii_lowercase();
    CLASSES.iter().any(|c| c.eq_ignore_ascii_case(class))
        || lower.starts_with("windowsforms10.edit.")
        || lower.starts_with("windowsforms10.richedit")
}

/// Почему `EM_REPLACESEL` не сработал.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceError {
    Gone,
    ReadOnly,
    /// поле не ответило (зависло) или Windows не пропустила сообщение (UIPI: окно с правами выше)
    NoAnswer(String),
    /// ответило, а длина текста не изменилась
    NoEffect,
}

fn send(field: HWND, msg: u32, wparam: usize, lparam: isize) -> Result<usize, String> {
    let mut out = 0usize;
    let r = unsafe {
        SendMessageTimeoutW(
            field,
            msg,
            WPARAM(wparam),
            LPARAM(lparam),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            SEND_TIMEOUT_MS,
            Some(&mut out),
        )
    };
    if r.0 == 0 {
        let e = unsafe { GetLastError() };
        return Err(format!("{e:?}"));
    }
    Ok(out)
}

/// Вставить текст в каретку классического поля, не трогая фокус. Проверка — по длине текста поля.
pub fn replace_sel(field: Hwnd, text: &str) -> Result<(), ReplaceError> {
    let h = hwnd(field);
    if !is_window(field) {
        return Err(ReplaceError::Gone);
    }
    let style = unsafe { GetWindowLongW(h, GWL_STYLE) } as u32;
    if style & ES_READONLY != 0 || !unsafe { IsWindowEnabled(h) }.as_bool() {
        return Err(ReplaceError::ReadOnly);
    }
    // поле Windows понимает перевод строки только как \r\n
    let crlf = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let wide: Vec<u16> = crlf.encode_utf16().chain(std::iter::once(0)).collect();
    let inserted = wide.len() as i64 - 1;
    let before = send(h, WM_GETTEXTLENGTH, 0, 0).map_err(ReplaceError::NoAnswer)? as i64;
    let sel = send(h, EM_GETSEL, 0, 0).map_err(ReplaceError::NoAnswer)?;
    let (start, end) = ((sel & 0xFFFF) as i64, ((sel >> 16) & 0xFFFF) as i64);
    // wParam 1 — правку можно отменить Ctrl+Z
    send(h, EM_REPLACESEL, 1, wide.as_ptr() as isize).map_err(ReplaceError::NoAnswer)?;
    let after = send(h, WM_GETTEXTLENGTH, 0, 0).map_err(ReplaceError::NoAnswer)? as i64;
    let selected = if end >= start && before < 0xFFFF { end - start } else { 0 };
    if after - before == inserted - selected || after != before {
        Ok(())
    } else {
        Err(ReplaceError::NoEffect)
    }
}

fn key_held(vk: i32) -> bool {
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

/// Нажата хоть одна клавиша или кнопка мыши.
pub fn any_key_held() -> bool {
    (0x01..=0xFE).filter(|vk| *vk != 0xE8).any(key_held)
}

/// Сколько миллисекунд не было нажатий клавиш. Время последней клавиши знают хук и Raw Input
/// (`hook::last_key_tick`); хук не запущен (пробник) — `GetLastInputInfo`, там и мышь.
pub fn keys_idle_ms() -> u32 {
    let now = unsafe { GetTickCount() };
    let last = hook::last_key_tick();
    if last != 0 {
        return now.wrapping_sub(last);
    }
    let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if unsafe { GetLastInputInfo(&mut lii) }.as_bool() {
        now.wrapping_sub(lii.dwTime)
    } else {
        u32::MAX
    }
}

/// Дождаться паузы в печати человека: клавиши и кнопки мыши отпущены, нажатий не было `quiet`.
/// Иначе его нажатия за время перескока ушли бы в чужое окно. false — паузы не было за `max`.
pub fn wait_input_pause(quiet: Duration, max: Duration) -> bool {
    let deadline = Instant::now() + max;
    loop {
        if !any_key_held() && keys_idle_ms() >= quiet.as_millis() as u32 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Очередь сообщений потока: без неё `AttachThreadInput` не работает.
pub fn ensure_message_queue() {
    let mut msg = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
}

/// Каким приёмом окно встало вперёд.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Front {
    Already,
    /// пустое событие мыши через SendInput, затем SetForegroundWindow (PowerToys)
    EmptyInput,
    /// AttachThreadInput к потоку переднего окна (AutoHotkey, PowerToys)
    Attach,
    /// отпускание Alt, которое глотает наш хук (AutoHotkey)
    AltUp,
}

impl Front {
    pub fn words(self) -> &'static str {
        match self {
            Front::Already => "уже впереди",
            Front::EmptyInput => "пустой ввод",
            Front::Attach => "AttachThreadInput",
            Front::AltUp => "Alt-up",
        }
    }
}

fn send_inputs(inputs: &[INPUT]) -> bool {
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) as usize == inputs.len() }
}

/// Пустое событие мыши: без движения и кнопок, но наш процесс становится тем, «кто дал последний
/// ввод», — ему разрешён `SetForegroundWindow` (PowerToys `WindowHelpers.BringToForeground`).
fn empty_mouse_input() -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dwExtraInfo: INJECTED_MARK, ..Default::default() } },
    };
    send_inputs(&[input])
}

/// Отпускание Alt с меткой, которую глотает наш хук: система ввод видит, приложения — нет
/// (AutoHotkey `SetForegroundWindowEx`, `KEY_BLOCK_THIS`).
fn swallowed_alt_up() -> bool {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_MENU.0),
                wScan: 0,
                dwFlags: KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: SWALLOWED_MARK,
            },
        },
    };
    send_inputs(&[input])
}

/// Окно впереди: оно само или его диалог (окно с владельцем `target` — владельца вперёд не поставить,
/// пока видим его диалог).
fn in_front(target: HWND) -> bool {
    let fg = unsafe { GetForegroundWindow() };
    fg == target || (!fg.is_invalid() && unsafe { GetWindow(fg, GW_OWNER) }.is_ok_and(|o| o == target))
}

fn wait_front(target: HWND, max: Duration) -> bool {
    let deadline = Instant::now() + max;
    loop {
        if in_front(target) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Поставить окно вперёд мимо ограничения `SetForegroundWindow` (foreground lock): фоновому
/// процессу Windows разрешает это, только если он дал последний ввод или подключён к потоку
/// переднего окна. Порядок приёмов — от безвредного к грубому. Свёрнутое окно сначала разворачивается.
pub fn bring_to_front(target: Hwnd) -> Option<Front> {
    let t = hwnd(target);
    if !is_window(target) || unsafe { IsHungAppWindow(t) }.as_bool() {
        return None;
    }
    if unsafe { IsIconic(t) }.as_bool() {
        // AutoHotkey: свёрнутое окно SetForegroundWindow не разворачивает
        unsafe {
            let _ = ShowWindow(t, SW_RESTORE);
        }
    }
    if in_front(t) {
        return Some(Front::Already);
    }
    let step = Duration::from_millis(60);
    empty_mouse_input();
    unsafe {
        let _ = SetForegroundWindow(t);
    }
    if wait_front(t, step) {
        return Some(Front::EmptyInput);
    }
    let fg = unsafe { GetForegroundWindow() };
    let me = unsafe { GetCurrentThreadId() };
    let fore_thread = if fg.is_invalid() { 0 } else { unsafe { GetWindowThreadProcessId(fg, None) } };
    if fore_thread != 0 && fore_thread != me && !unsafe { IsHungAppWindow(fg) }.as_bool() {
        ensure_message_queue();
        let attached = unsafe { AttachThreadInput(me, fore_thread, true) }.as_bool();
        unsafe {
            let _ = SetForegroundWindow(t);
        }
        if attached {
            unsafe {
                let _ = AttachThreadInput(me, fore_thread, false);
            }
        }
        if wait_front(t, step) {
            return Some(Front::Attach);
        }
    }
    swallowed_alt_up();
    unsafe {
        let _ = SetForegroundWindow(t);
    }
    if wait_front(t, step) {
        return Some(Front::AltUp);
    }
    None
}

/// Фокус клавиатуры на окно-поле `field` в чужом потоке: `SetFocus` работает только для окон
/// своего потока, поэтому на время вызова очереди ввода потоков объединяются.
pub fn focus_hwnd(field: Hwnd, thread: u32) -> bool {
    let me = unsafe { GetCurrentThreadId() };
    if !is_window(field) || thread == 0 || thread == me || unsafe { IsHungAppWindow(hwnd(field)) }.as_bool() {
        return false;
    }
    ensure_message_queue();
    unsafe {
        if !AttachThreadInput(me, thread, true).as_bool() {
            return false;
        }
        let _ = SetFocus(Some(hwnd(field)));
        let _ = AttachThreadInput(me, thread, false);
    }
    focus_of(thread).0 == field
}

pub fn is_iconic(h: Hwnd) -> bool {
    unsafe { IsIconic(hwnd(h)) }.as_bool()
}

/// Свернуть обратно, не трогая переднее окно.
pub fn minimize_quietly(h: Hwnd) {
    unsafe {
        let _ = ShowWindowAsync(hwnd(h), SW_SHOWMINNOACTIVE);
    }
}

/// Верхнее окно для окна-поля.
pub fn root_of(h: Hwnd) -> Hwnd {
    raw(unsafe { GetAncestor(hwnd(h), GA_ROOT) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_edit_classes() {
        for c in ["Edit", "RichEdit20W", "RICHEDIT50W", "RichEditD2DPT", "WindowsForms10.EDIT.app.0.141b42a_r6_ad1"] {
            assert!(is_classic_edit(c), "{c}");
        }
        for c in ["Chrome_WidgetWin_1", "Chrome_RenderWidgetHostHWND", "Qt51518QWindowIcon", ""] {
            assert!(!is_classic_edit(c), "{c}");
        }
    }

    #[test]
    fn fullscreen_is_covering_the_monitor() {
        let m = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        assert!(covers(&RECT { left: 0, top: 0, right: 1920, bottom: 1080 }, &m));
        assert!(covers(&RECT { left: -8, top: -8, right: 1928, bottom: 1088 }, &m));
        // развёрнутое окно с видимой панелью задач монитор не закрывает
        assert!(!covers(&RECT { left: -8, top: -8, right: 1928, bottom: 1048 }, &m));
    }
}
