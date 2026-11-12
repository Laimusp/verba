//! Трей на Win32 напрямую (`Shell_NotifyIconW`): значок состояния, меню, всплывающие уведомления.
//!
//! Почему не крейт `tray-icon` (CLAUDE.md «Стек»): у него нет всплывающих уведомлений
//! (`NIF_INFO`), а до оверлея (T-004) ошибки «что не так → что сделать» показать больше нечем.
//! Своё — это ещё и повторное добавление значка после перезапуска Проводника (`TaskbarCreated`),
//! тёмное меню по теме системы и ни одной зависимости.
//!
//! Поток трея ничего не ждёт (CLAUDE.md §2): состояние берёт под коротким локом, команды меню
//! отправляет каналом, файлы открывает из отдельного потока.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_NONE, NIIF_NOSOUND, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenuEx, TranslateMessage, HICON, HMENU, ICONINFO, MF_CHECKED,
    MF_GRAYED, MF_POPUP, MF_STRING, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, SM_CXSMICON, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_NULL,
    WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP, WTS_SESSION_LOCK,
    WTS_SESSION_UNLOCK,
};

use crate::config::Engine;
use crate::icons::{self, IconState};

pub const WINDOW_CLASS: PCWSTR = w!("VerbaTray");
const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;
/// второй экземпляр Verba просит показать, что первый уже работает
pub const WM_SECOND_INSTANCE: u32 = WM_APP + 3;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const TRAY_ID: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Ready,
    Recording,
    Transcribing,
}

/// Что показывает трей. Пишет контроллер, читает поток трея.
#[derive(Clone, Debug, Default)]
pub struct UiState {
    pub status: Status,
    pub paused: bool,
    /// последняя ошибка (значок с красной точкой), пока не пройдёт удачная диктовка
    pub error: Option<String>,
    pub engine: String,
    /// выбран движок «ChatGPT по входу Codex»
    pub codex: bool,
    pub model: String,
    pub models: Vec<String>,
    /// последние диктовки, новые сверху
    pub history: Vec<String>,
    pub pause_label: String,
    pub dictate_label: String,
    pub paste_last_label: String,
    pub has_key: bool,
    pub proxy: Option<String>,
    /// уведомления к показу
    pub balloons: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum MenuCmd {
    TogglePause,
    SelectEngine(Engine),
    /// «Проверить движок»: путь до сервиса целиком, итог — уведомлением
    CheckEngine,
    SelectModel(usize),
    CopyHistory(usize),
    OpenConfig,
    OpenLog,
    KeyFromClipboard,
    ProxyFromClipboard,
    Exit,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Menu(MenuCmd),
    Lock,
    Unlock,
    Resume,
}

#[derive(Clone)]
pub struct UiHandle {
    hwnd: isize,
    state: Arc<Mutex<UiState>>,
}

impl UiHandle {
    pub fn update(&self, f: impl FnOnce(&mut UiState)) {
        if let Ok(mut s) = self.state.lock() {
            f(&mut s);
        }
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_REFRESH, WPARAM(0), LPARAM(0));
        }
    }

    pub fn notify(&self, text: impl Into<String>) {
        let text = text.into();
        self.update(|s| s.balloons.push(text));
    }

    pub fn quit(&self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

struct Ctx {
    hwnd: HWND,
    state: Arc<Mutex<UiState>>,
    tx: Sender<UiEvent>,
    icons: HashMap<(IconState, u32, bool), HICON>,
    shown: Option<(IconState, String)>,
    dark: bool,
    taskbar_created: u32,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

fn wide_into<const N: usize>(dst: &mut [u16; N], s: &str) {
    let mut n = 0;
    for c in s.encode_utf16() {
        if n + 1 >= N {
            break;
        }
        dst[n] = c;
        n += 1;
    }
    dst[n] = 0;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Тема панели задач: true — тёмная (значок светлый).
fn taskbar_is_dark() -> bool {
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    // нет значения (старая Windows) — панель тёмная
    r.is_err() || value == 0
}

/// Меню по теме системы: недокументированные `SetPreferredAppMode` (uxtheme #135) и
/// `FlushMenuThemes` (#136), Windows 10 1903+. Так делают Notepad++, PowerToys и др.;
/// нет функций — меню остаётся светлым, ничего не ломается.
fn follow_system_theme_for_menus() {
    unsafe {
        let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(f) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
            let set_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_mode(1); // AllowDark: как в системе
        }
        if let Some(f) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
            let flush: extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

fn icon_size() -> u32 {
    unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()).max(16) as u32 }
}

fn make_icon(state: IconState, size: u32, dark: bool) -> Option<HICON> {
    let rgba = icons::render(state, size, dark);
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(rgba.as_chunks::<4>().0) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
        let zeros = vec![0u8; (size.div_ceil(16) * 2 * size) as usize];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, Some(zeros.as_ptr() as *const _));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    }
}

fn status_words(s: &UiState) -> &'static str {
    match s.status {
        Status::Recording => "запись",
        Status::Transcribing => "распознаю",
        Status::Ready if s.paused => "пауза",
        Status::Ready if s.error.is_some() => "ошибка",
        Status::Ready => "готов",
    }
}

fn icon_state(s: &UiState) -> IconState {
    match s.status {
        Status::Recording => IconState::Recording,
        Status::Transcribing => IconState::Transcribing,
        Status::Ready if s.paused => IconState::Paused,
        Status::Ready if s.error.is_some() => IconState::Error,
        Status::Ready => IconState::Ready,
    }
}

fn short_model(m: &str) -> &str {
    m.rsplit('/').next().unwrap_or(m)
}

/// Чем распознаёт: модель OpenRouter или ChatGPT.
fn engine_words(s: &UiState) -> String {
    if s.codex {
        "ChatGPT по входу Codex".into()
    } else {
        format!("{} · {}", s.engine, short_model(&s.model))
    }
}

impl Ctx {
    fn icon(&mut self, st: IconState) -> HICON {
        let key = (st, icon_size(), self.dark);
        if let Some(h) = self.icons.get(&key) {
            return *h;
        }
        let h = make_icon(st, key.1, key.2).unwrap_or_default();
        self.icons.insert(key, h);
        h
    }

    fn add(&mut self) {
        let st = self.state.lock().map(|s| icon_state(&s)).unwrap_or(IconState::Ready);
        let mut d = nid(self.hwnd);
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icon(st);
        wide_into(&mut d.szTip, "Verba");
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &d);
            d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
        }
        self.shown = None;
        self.refresh();
    }

    fn refresh(&mut self) {
        let (st, tip, balloons) = {
            let Ok(mut s) = self.state.lock() else { return };
            let mut tip = format!("Verba — {}", status_words(&s));
            if !s.model.is_empty() {
                tip.push_str(&format!("\n{}", engine_words(&s)));
            }
            if let Some(e) = &s.error {
                tip.push_str(&format!("\n{e}"));
            }
            (icon_state(&s), tip, std::mem::take(&mut s.balloons))
        };
        if self.shown.as_ref() != Some(&(st, tip.clone())) {
            let mut d = nid(self.hwnd);
            d.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
            d.hIcon = self.icon(st);
            wide_into(&mut d.szTip, &tip);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
            }
            self.shown = Some((st, tip));
        }
        // несколько уведомлений подряд — показываем последнее: Windows всё равно заменяет предыдущее
        if let Some(text) = balloons.last() {
            let mut d = nid(self.hwnd);
            d.uFlags = NIF_INFO;
            d.dwInfoFlags = NIIF_NONE | NIIF_NOSOUND;
            wide_into(&mut d.szInfoTitle, "Verba");
            wide_into(&mut d.szInfo, text);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
            }
        }
    }

    fn theme_changed(&mut self) {
        let dark = taskbar_is_dark();
        follow_system_theme_for_menus();
        if dark != self.dark {
            self.dark = dark;
            self.shown = None;
            self.refresh();
        }
    }

    fn show_menu(&mut self) {
        let s = match self.state.lock() {
            Ok(s) => s.clone(),
            Err(_) => return,
        };
        let esc = |t: &str| t.replace('&', "&&");
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let add = |m: HMENU, flags, id: usize, text: &str| {
                let t = wide(text);
                let _ = AppendMenuW(m, flags, id, PCWSTR(t.as_ptr()));
            };
            let head = match &s.error {
                Some(e) if s.status == Status::Ready && !s.paused => format!("Verba · {e}"),
                _ => format!("Verba · {} · {}", status_words(&s), if s.codex { "ChatGPT" } else { short_model(&s.model) }),
            };
            let head: String = head.chars().take(90).collect();
            add(menu, MF_STRING | MF_GRAYED, 1, &esc(&head));
            if !s.has_key {
                add(menu, MF_STRING, 30, "Ключ OpenRouter из буфера обмена");
            }
            add(menu, MF_STRING | if s.paused { MF_CHECKED } else { MF_STRING }, 2, &format!("Пауза\t{}", s.pause_label));

            if let Ok(models) = CreatePopupMenu() {
                let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
                add(models, check(!s.codex), 10, "OpenRouter");
                for (i, m) in s.models.iter().enumerate().take(50) {
                    add(models, check(!s.codex && *m == s.model), 100 + i, &esc(&format!("      {m}")));
                }
                add(models, check(s.codex), 11, "ChatGPT по входу Codex");
                add(models, MF_STRING, 30, "Ключ OpenRouter из буфера обмена");
                let proxy = match &s.proxy {
                    Some(p) => format!("Прокси из буфера обмена (сейчас {p})"),
                    None => "Прокси из буфера обмена".into(),
                };
                add(models, MF_STRING, 31, &esc(&proxy));
                add(menu, MF_POPUP, models.0 as usize, "Движок и модель");
            }
            add(menu, MF_STRING, 32, "Проверить движок");
            if let Ok(hist) = CreatePopupMenu() {
                if s.history.is_empty() {
                    add(hist, MF_STRING | MF_GRAYED, 8, "Диктовок пока нет");
                }
                for (i, h) in s.history.iter().enumerate().take(50) {
                    let mut preview: String = h.chars().take(48).collect();
                    if h.chars().count() > 48 {
                        preview.push('…');
                    }
                    add(hist, MF_STRING, 200 + i, &esc(&preview));
                }
                add(menu, MF_POPUP, hist.0 as usize, &format!("Последние диктовки\t{}", s.paste_last_label));
            }
            add(menu, MF_STRING, 20, "Открыть настройки");
            add(menu, MF_STRING, 21, "Открыть лог");
            add(menu, MF_STRING, 99, "Выход");

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // без этого меню не закрывается щелчком мимо (документация TrackPopupMenu)
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY).0, pt.x, pt.y, self.hwnd, None).0
                as usize;
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            let cmd = match cmd {
                2 => Some(MenuCmd::TogglePause),
                10 => Some(MenuCmd::SelectEngine(Engine::OpenRouter)),
                11 => Some(MenuCmd::SelectEngine(Engine::Codex)),
                32 => Some(MenuCmd::CheckEngine),
                20 => Some(MenuCmd::OpenConfig),
                21 => Some(MenuCmd::OpenLog),
                30 => Some(MenuCmd::KeyFromClipboard),
                31 => Some(MenuCmd::ProxyFromClipboard),
                99 => Some(MenuCmd::Exit),
                100..=149 => Some(MenuCmd::SelectModel(cmd - 100)),
                200..=249 => Some(MenuCmd::CopyHistory(cmd - 200)),
                _ => None,
            };
            if let Some(c) = cmd {
                let _ = self.tx.send(UiEvent::Menu(c));
            }
        }
    }
}

fn with_ctx(f: impl FnOnce(&mut Ctx)) {
    CTX.with(|c| {
        if let Ok(mut c) = c.try_borrow_mut() {
            if let Some(ctx) = c.as_mut() {
                f(ctx);
            }
        }
    });
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_CONTEXTMENU || event == NIN_SELECT || event == NIN_KEYSELECT || event == WM_LBUTTONUP {
                // пока меню открыто, обновления не применяются (контекст занят) — применить после
                with_ctx(|c| {
                    c.show_menu();
                    c.refresh();
                });
            }
            LRESULT(0)
        }
        WM_REFRESH => {
            with_ctx(|c| c.refresh());
            LRESULT(0)
        }
        WM_SECOND_INSTANCE => {
            with_ctx(|c| {
                if let Ok(mut s) = c.state.lock() {
                    s.balloons.push("Verba уже работает — значок в трее".into());
                }
                c.refresh();
            });
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let what = if lparam.0 != 0 { PCWSTR(lparam.0 as *const u16).to_string().unwrap_or_default() } else { String::new() };
            if what == "ImmersiveColorSet" {
                with_ctx(|c| c.theme_changed());
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_WTSSESSION_CHANGE => {
            let ev = match wparam.0 as u32 {
                WTS_SESSION_LOCK => Some(UiEvent::Lock),
                WTS_SESSION_UNLOCK => Some(UiEvent::Unlock),
                _ => None,
            };
            if let Some(ev) = ev {
                with_ctx(|c| {
                    let _ = c.tx.send(ev);
                });
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            let w = wparam.0 as u32;
            if w == PBT_APMRESUMEAUTOMATIC || w == PBT_APMRESUMESUSPEND {
                with_ctx(|c| {
                    let _ = c.tx.send(UiEvent::Resume);
                });
            }
            LRESULT(1)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            with_ctx(|c| {
                let d = nid(c.hwnd);
                let _ = Shell_NotifyIconW(NIM_DELETE, &d);
                for (_, h) in c.icons.drain() {
                    let _ = DestroyIcon(h);
                }
            });
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let mut handled = false;
            with_ctx(|c| {
                // Проводник перезапустился — значок надо добавить заново
                if c.taskbar_created != 0 && msg == c.taskbar_created {
                    c.add();
                    handled = true;
                }
            });
            if handled {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}

/// Создаёт окно трея на текущем (главном) потоке. Цикл сообщений — `run_loop`.
pub fn create(state: UiState, tx: Sender<UiEvent>) -> Result<UiHandle, String> {
    unsafe {
        follow_system_theme_for_menus();
        let hinst = HINSTANCE(GetModuleHandleW(None).map_err(|e| e.to_string())?.0);
        let wc = WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: hinst, lpszClassName: WINDOW_CLASS, ..Default::default() };
        RegisterClassW(&wc);
        // обычное скрытое окно, не message-only: иначе не придёт широковещательное TaskbarCreated
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, WINDOW_CLASS, w!("Verba"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst), None)
            .map_err(|e| format!("окно трея: {e}"))?;
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        let state = Arc::new(Mutex::new(state));
        let ctx = Ctx {
            hwnd,
            state: state.clone(),
            tx,
            icons: HashMap::new(),
            shown: None,
            dark: taskbar_is_dark(),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        };
        CTX.with(|c| *c.borrow_mut() = Some(ctx));
        with_ctx(|c| c.add());
        Ok(UiHandle { hwnd: hwnd.0 as isize, state })
    }
}

/// Цикл сообщений главного потока; возвращается после закрытия окна трея.
pub fn run_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
