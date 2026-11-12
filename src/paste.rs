//! Вставка текста в текущий фокус: буфер обмена → Ctrl+V → прежнее содержимое буфера обратно.
//!
//! Перенос из Handy (MIT): `handy-098\src-tauri\src\paste_tx\windows.rs` и `paste_tx\mod.rs`
//! (`evaluate`, константы ожидания). Механизм:
//! 1. снимок буфера (все форматы, кроме не-памяти GDI) — окно потока вставки становится владельцем;
//! 2. текст кладётся «обещанием» (`SetClipboardData(CF_UNICODETEXT, NULL)`, отложенная отдача):
//!    Windows присылает владельцу `WM_RENDERFORMAT`, когда приложение реально читает буфер, — это
//!    квитанция о прочтении;
//! 3. Ctrl+V; когда чтения стихли (200 мс после последнего) или прошло 8 с — прежнее содержимое
//!    возвращается, если буфер с тех пор никто не менял (номер последовательности буфера);
//! 4. маркеры `ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory = 0`,
//!    `CanUploadToCloudClipboard = 0`: текст диктовки не попадает в журнал Win+V и облако.
//!
//! Вставка «на взводе» (`paste_armed`) — для вставки в запомненное поле (`target`): шаги 1–2 до
//! перевода чужого окна вперёд, Ctrl+V — по команде `Armed::fire`; вехи (Ctrl+V ушёл, первое
//! чтение) говорят, когда можно вернуть фокус человеку.

use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, SetLastError, ERROR_SUCCESS, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner, GetClipboardSequenceNumber,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    CF_BITMAP, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT, CF_DSPTEXT, CF_ENHMETAFILE, CF_OWNERDISPLAY, CF_PALETTE,
    CF_UNICODETEXT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY, VK_CONTROL, VK_INSERT, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    CopyImage, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW, GetMessageW,
    GetWindowLongPtrW, GetWindowThreadProcessId, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, SetTimer,
    SetWindowLongPtrW, GDI_IMAGE_TYPE, GWLP_USERDATA, HWND_MESSAGE, IMAGE_FLAGS, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_TIMER, WNDCLASSW,
};

use crate::config::PasteMethod;
use crate::hotkey::hook::INJECTED_MARK;

/// Чтения стихли столько — прочитали (Chromium читает буфер дважды: проба, потом чтение).
const QUIET_PERIOD: Duration = Duration::from_millis(200);
/// Дольше текст в буфере не держим, даже без квитанции.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(8);
/// Ctrl+V не ушёл — квитанции не будет, возвращаем быстро.
const FAILED_INJECTION_TIMEOUT: Duration = Duration::from_millis(500);
/// Удержание сочетания: слишком быстрое Ctrl+V некоторые системы теряли (Handy #165).
/// Вставка на взводе отпускает клавиши раньше — как только приложение прочитало буфер: значит,
/// Ctrl+V оно уже увидело, а чужое окно впереди лишние ~90 мс не держится.
const CHORD_HOLD_MS: u64 = 100;
/// Модификаторы человека (Alt от Ctrl+Alt+V) испортили бы наш Ctrl+V — ждём отпускания столько.
const MODIFIERS_WAIT: Duration = Duration::from_millis(1500);
const MAX_FORMAT_BYTES: usize = 64 * 1024 * 1024;
const TIMER_ID: usize = 1;
const TIMER_INTERVAL_MS: u32 = 25;
/// Вставка на взводе не дождалась команды — буфер возвращается человеку.
const ARM_TIMEOUT: Duration = Duration::from_secs(5);
/// Команда окну транзакции: wParam 1 — жать Ctrl+V, 0 — отмена.
const WM_GO: u32 = WM_APP + 7;
const CLASS_NAME: &str = "VerbaPasteTx";

#[derive(Debug, Clone)]
pub struct PasteReport {
    /// от начала вставки до отправки Ctrl+V (включая ожидание модификаторов и команды «жать»)
    pub chord_ms: u64,
    /// приложение прочитало буфер через столько после Ctrl+V; None — не прочитало
    pub read_ms: Option<u64>,
    pub restored: bool,
    pub error: Option<String>,
    /// вставку на взводе отменил тот, кто её взвёл: Ctrl+V не нажимался
    pub aborted: bool,
}

/// Вехи вставки — для того, кто держит чужое окно впереди, пока идёт Ctrl+V (`target`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Milestone {
    /// буфер готов, ждём команды (`Armed`); число — окно транзакции
    Ready(isize),
    /// Ctrl+V отправлен целиком: нажатие и отпускание
    ChordSent,
    ChordFailed,
    /// приложение впервые прочитало буфер после Ctrl+V
    FirstRead,
    /// транзакция кончилась, итог ушёл в `done`
    Ended,
}

/// Вставка на взводе: буфер уже подготовлен, Ctrl+V — по команде.
pub struct Armed(isize);

impl Armed {
    pub fn new(tx_window: isize) -> Self {
        Armed(tx_window)
    }
    pub fn fire(&self) {
        self.post(1);
    }
    pub fn cancel(&self) {
        self.post(0);
    }
    fn post(&self, go: usize) {
        let hwnd = HWND(self.0 as *mut _);
        // окно транзакции могло закрыться, а его номер — достаться чужому окну: шлём только своему
        let mut pid = 0;
        let mut class = [0u16; 32];
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let n = GetClassNameW(hwnd, &mut class).max(0) as usize;
            if pid == GetCurrentProcessId() && String::from_utf16_lossy(&class[..n]) == CLASS_NAME {
                let _ = PostMessageW(Some(hwnd), WM_GO, WPARAM(go), LPARAM(0));
            }
        }
    }
}

struct TxState {
    published_at: Instant,
    injected_at: Option<Instant>,
    injection_failed: bool,
    receipts: Vec<Instant>,
    ownership_lost: bool,
    cancelled: bool,
    /// ждёт команды «жать» (вставка на взводе)
    armed: bool,
    aborted: bool,
    first_read_sent: bool,
}

impl TxState {
    fn new() -> Self {
        Self {
            published_at: Instant::now(),
            injected_at: None,
            injection_failed: false,
            receipts: Vec::new(),
            ownership_lost: false,
            cancelled: false,
            armed: false,
            aborted: false,
            first_read_sent: false,
        }
    }
    fn last_receipt_after_injection(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.receipts.iter().copied().rev().find(|t| *t >= injected)
    }
    fn first_receipt_after_injection(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.receipts.iter().copied().find(|t| *t >= injected)
    }
}

fn should_finish(s: &TxState, now: Instant) -> bool {
    if s.ownership_lost || s.cancelled {
        return true;
    }
    if s.armed && s.injected_at.is_none() {
        return now.duration_since(s.published_at) >= ARM_TIMEOUT;
    }
    if let Some(last) = s.last_receipt_after_injection() {
        if now.duration_since(last) >= QUIET_PERIOD {
            return true;
        }
    }
    let deadline = if s.injection_failed { FAILED_INJECTION_TIMEOUT } else { RESTORE_TIMEOUT };
    now.duration_since(s.published_at) >= deadline
}

struct SavedFormat {
    format: u32,
    data: Vec<u8>,
}

type Done = Box<dyn FnOnce(PasteReport) + Send>;

struct Shared {
    state: Mutex<TxState>,
    text: Vec<u16>,
    snapshot: Mutex<Vec<SavedFormat>>,
    saved_bitmap: Mutex<Option<usize>>,
    sequence: Mutex<u32>,
    /// вернуть прежнее содержимое буфера
    restore: bool,
    /// текст «обещанием» и ждать квитанций чтения (нужно возврату буфера и вехам)
    wait_reads: bool,
    /// Ctrl+V — только по команде `Armed::fire`
    armed: bool,
    method: PasteMethod,
    watch: Option<Sender<Milestone>>,
    /// первое чтение после Ctrl+V — сигнал отпустить клавиши (вставка на взводе)
    first_read: (Mutex<bool>, Condvar),
    started: Instant,
    chord_ms: Mutex<u64>,
    done: Mutex<Option<Done>>,
}

impl Shared {
    /// Держать Ctrl+V: на взводе — до первого чтения буфера, но не дольше `CHORD_HOLD_MS`.
    fn hold_chord(&self) {
        let max = Duration::from_millis(CHORD_HOLD_MS);
        if !self.armed {
            return std::thread::sleep(max);
        }
        let (lock, cv) = &self.first_read;
        let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ = cv.wait_timeout_while(guard, max, |read| !*read);
    }

    fn note(&self, m: Milestone) {
        if let Some(w) = &self.watch {
            let _ = w.send(m);
        }
    }
}

/// Транзакция, которая сейчас держит буфер: новая вставка сначала завершает её.
static PENDING: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn shared_ptr(hwnd: HWND) -> *const Shared {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared
}

/// Кладёт текст в уже открытый буфер.
unsafe fn render_text(text: &[u16]) {
    let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2) else { return };
    let ptr = GlobalLock(hg) as *mut u16;
    if ptr.is_null() {
        let _ = GlobalFree(Some(hg));
        return;
    }
    std::ptr::copy_nonoverlapping(text.as_ptr(), ptr, text.len());
    let _ = GlobalUnlock(hg);
    if SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(hg.0))).is_err() {
        let _ = GlobalFree(Some(hg));
    }
}

/// Маркеры «не в журнал буфера, не в облако, мониторам не показывать» — в открытый буфер.
unsafe fn set_private_markers() {
    for (name, value) in
        [("ExcludeClipboardContentFromMonitorProcessing", 1u32), ("CanIncludeInClipboardHistory", 0), ("CanUploadToCloudClipboard", 0)]
    {
        let name_w = wide(name);
        let format = RegisterClipboardFormatW(PCWSTR(name_w.as_ptr()));
        if format == 0 {
            continue;
        }
        if let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>()) {
            let ptr = GlobalLock(hg) as *mut u32;
            if ptr.is_null() {
                let _ = GlobalFree(Some(hg));
                continue;
            }
            *ptr = value;
            let _ = GlobalUnlock(hg);
            if SetClipboardData(format, Some(HANDLE(hg.0))).is_err() {
                let _ = GlobalFree(Some(hg));
            }
        }
    }
}

/// Открыть буфер: его на мгновение держат другие программы — несколько попыток.
unsafe fn open_clipboard(owner: Option<HWND>) -> bool {
    for _ in 0..20 {
        if OpenClipboard(owner).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

unsafe fn restore_snapshot(shared: &Shared) -> bool {
    if !open_clipboard(None) {
        crate::warn!("вставка: буфер занят другой программой — прежнее содержимое не вернул");
        return false;
    }
    let _ = EmptyClipboard();
    if let Ok(formats) = shared.snapshot.lock() {
        for saved in formats.iter().filter(|f| !f.data.is_empty()) {
            let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, saved.data.len()) else { continue };
            let ptr = GlobalLock(hg) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(Some(hg));
                continue;
            }
            std::ptr::copy_nonoverlapping(saved.data.as_ptr(), ptr, saved.data.len());
            let _ = GlobalUnlock(hg);
            if SetClipboardData(saved.format, Some(HANDLE(hg.0))).is_err() {
                let _ = GlobalFree(Some(hg));
            }
        }
    }
    if let Ok(mut bitmap) = shared.saved_bitmap.lock() {
        if let Some(raw) = bitmap.take() {
            let _ = SetClipboardData(u32::from(CF_BITMAP.0), Some(HANDLE(raw as *mut _)));
        }
    }
    // возвращённое содержимое уже было в журнале Win+V — второй раз его туда не класть
    set_private_markers();
    let _ = CloseClipboard();
    true
}

unsafe fn snapshot_clipboard(hwnd: HWND, shared: &Shared) -> Result<(), String> {
    if !open_clipboard(Some(hwnd)) {
        return Err("буфер обмена занят другой программой".into());
    }
    let mut formats = Vec::new();
    let mut format = 0u32;
    loop {
        format = EnumClipboardFormats(format);
        if format == 0 {
            break;
        }
        if format == u32::from(CF_BITMAP.0) {
            // объект GDI, не память: копируем картинку
            if let Ok(handle) = GetClipboardData(u32::from(CF_BITMAP.0)) {
                if let Ok(copy) = CopyImage(handle, GDI_IMAGE_TYPE(0), 0, 0, IMAGE_FLAGS(0x2000)) {
                    if let Ok(mut slot) = shared.saved_bitmap.lock() {
                        *slot = Some(copy.0 as usize);
                    }
                }
            }
            continue;
        }
        let not_memory = [
            CF_ENHMETAFILE.0,
            CF_DSPENHMETAFILE.0,
            CF_DSPBITMAP.0,
            CF_DSPMETAFILEPICT.0,
            CF_DSPTEXT.0,
            CF_OWNERDISPLAY.0,
            CF_PALETTE.0,
        ];
        if not_memory.iter().any(|f| u32::from(*f) == format) {
            continue;
        }
        if let Ok(handle) = GetClipboardData(format) {
            let hg = HGLOBAL(handle.0);
            let size = GlobalSize(hg);
            if size == 0 || size > MAX_FORMAT_BYTES {
                continue;
            }
            let ptr = GlobalLock(hg) as *const u8;
            if ptr.is_null() {
                continue;
            }
            let data = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(hg);
            formats.push(SavedFormat { format, data });
        }
    }
    let _ = CloseClipboard();
    if let Ok(mut slot) = shared.snapshot.lock() {
        *slot = formats;
    }
    Ok(())
}

/// Текст — «обещанием» (отдаётся по запросу) или сразу, если возвращать буфер не нужно.
unsafe fn publish(hwnd: HWND, shared: &Shared) -> Result<u32, String> {
    if !open_clipboard(Some(hwnd)) {
        return Err("буфер обмена занят другой программой".into());
    }
    let result = (|| {
        EmptyClipboard().map_err(|e| format!("EmptyClipboard: {e}"))?;
        set_private_markers();
        if shared.wait_reads {
            // NULL — отложенная отдача; успех тоже NULL, ошибку видно только по GetLastError
            SetLastError(ERROR_SUCCESS);
            if let Err(e) = SetClipboardData(u32::from(CF_UNICODETEXT.0), None) {
                if e.code().is_err() {
                    return Err(format!("SetClipboardData: {e}"));
                }
            }
        } else {
            render_text(&shared.text);
        }
        Ok(())
    })();
    let closed = CloseClipboard();
    result?;
    closed.map_err(|e| format!("CloseClipboard: {e}"))?;
    Ok(GetClipboardSequenceNumber())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let shared = shared_ptr(hwnd);
    match msg {
        WM_RENDERFORMAT => {
            if !shared.is_null() {
                let shared = &*shared;
                let mut first = false;
                if let Ok(mut st) = shared.state.lock() {
                    let now = Instant::now();
                    st.receipts.push(now);
                    if !st.first_read_sent && st.injected_at.is_some_and(|i| now >= i) {
                        st.first_read_sent = true;
                        first = true;
                        let (lock, cv) = &shared.first_read;
                        *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
                        cv.notify_all();
                    }
                }
                if wparam.0 as u32 == u32::from(CF_UNICODETEXT.0) {
                    render_text(&shared.text);
                }
                if first {
                    shared.note(Milestone::FirstRead);
                }
            }
            LRESULT(0)
        }
        WM_GO => {
            if !shared.is_null() {
                if wparam.0 == 1 {
                    // указатель живёт, пока живо окно; свой Arc — для потока Ctrl+V
                    Arc::increment_strong_count(shared);
                    start_chord(&Arc::from_raw(shared));
                } else if let Ok(mut st) = (*shared).state.lock() {
                    if st.injected_at.is_none() {
                        st.cancelled = true;
                        st.aborted = true;
                    }
                }
            }
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            // окно закрывается с невыполненным обещанием — не чтение, квитанции нет
            if !shared.is_null() && OpenClipboard(Some(hwnd)).is_ok() {
                if GetClipboardOwner().map(|o| o == hwnd).unwrap_or(false) {
                    render_text(&(*shared).text);
                }
                let _ = CloseClipboard();
            }
            LRESULT(0)
        }
        WM_DESTROYCLIPBOARD => {
            if !shared.is_null() {
                if let Ok(mut st) = (*shared).state.lock() {
                    st.ownership_lost = true;
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if !shared.is_null() {
                on_timer(&*shared);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn settle(shared: &Shared) -> bool {
    let sequence = *shared.sequence.lock().unwrap_or_else(|p| p.into_inner());
    let lost = shared.state.lock().map(|s| s.ownership_lost).unwrap_or(true);
    let still_ours = !lost && unsafe { GetClipboardSequenceNumber() } == sequence;
    if shared.restore && still_ours {
        unsafe { restore_snapshot(shared) }
    } else {
        false
    }
}

fn report(shared: &Shared, restored: bool, error: Option<String>) {
    let (read_ms, aborted) = shared
        .state
        .lock()
        .map(|s| {
            let read = s.injected_at.and_then(|i| Some(s.first_receipt_after_injection()?.duration_since(i).as_millis() as u64));
            (read, s.aborted)
        })
        .unwrap_or((None, false));
    let chord_ms = *shared.chord_ms.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(done) = shared.done.lock().ok().and_then(|mut d| d.take()) {
        done(PasteReport { chord_ms, read_ms, restored, error, aborted });
        shared.note(Milestone::Ended);
    }
}

/// Ctrl+V — из отдельного потока: поток транзакции должен качать сообщения, пока приложение
/// читает буфер. Второй вызов ничего не делает.
fn start_chord(shared: &Arc<Shared>) {
    if let Ok(mut st) = shared.state.lock() {
        if st.injected_at.is_some() || st.cancelled {
            return;
        }
        st.published_at = Instant::now();
        // отметка до отправки: быстрое приложение читает, пока сочетание ещё нажато
        st.injected_at = Some(Instant::now());
    }
    *shared.chord_ms.lock().unwrap_or_else(|p| p.into_inner()) = shared.started.elapsed().as_millis() as u64;
    let chord_shared = shared.clone();
    let spawned = std::thread::Builder::new().name("verba-chord".into()).spawn(move || match send_chord(chord_shared.method, || chord_shared.hold_chord()) {
        Ok(()) => chord_shared.note(Milestone::ChordSent),
        Err(e) => {
            crate::warn!("вставка: {e}");
            if let Ok(mut st) = chord_shared.state.lock() {
                st.injection_failed = true;
            }
            chord_shared.note(Milestone::ChordFailed);
        }
    });
    if spawned.is_err() {
        if let Ok(mut st) = shared.state.lock() {
            st.injection_failed = true;
        }
        shared.note(Milestone::ChordFailed);
    }
}

fn on_timer(shared: &Shared) {
    let finish = match shared.state.lock() {
        Ok(mut st) => {
            let f = should_finish(&st, Instant::now());
            if f {
                st.cancelled = true;
                // взвели и не нажали — вставки не было
                if st.armed && st.injected_at.is_none() {
                    st.aborted = true;
                }
            }
            f
        }
        Err(_) => true,
    };
    if !finish {
        return;
    }
    let restored = settle(shared);
    if let Ok(mut slot) = PENDING.lock() {
        if slot.as_ref().is_some_and(|p| std::ptr::eq(Arc::as_ptr(p), shared)) {
            *slot = None;
        }
    }
    report(shared, restored, None);
    unsafe { PostQuitMessage(0) };
}

/// Прошлая вставка ещё держит буфер — завершить её сейчас, чтобы снимок взял буфер человека.
fn flush_pending() {
    let previous = PENDING.lock().ok().and_then(|mut s| s.take());
    if let Some(prev) = previous {
        if let Ok(mut st) = prev.state.lock() {
            st.cancelled = true;
            if st.armed && st.injected_at.is_none() {
                st.aborted = true;
            }
        }
        let restored = settle(&prev);
        report(&prev, restored, None);
    }
}

fn ensure_class(hinst: HINSTANCE) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // имя — то же, что CLASS_NAME (по нему Armed узнаёт своё окно)
        let wc =
            WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: hinst, lpszClassName: w!("VerbaPasteTx"), ..Default::default() };
        unsafe {
            RegisterClassW(&wc);
        }
    });
}

fn held(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(i32::from(vk.0)) as u16 & 0x8000) != 0 }
}

fn user_modifiers_held() -> bool {
    [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN].into_iter().any(held)
}

fn send_chord(method: PasteMethod, hold: impl FnOnce()) -> Result<(), String> {
    const VK_V: u16 = 0x56;
    let (mods, key): (&[VIRTUAL_KEY], VIRTUAL_KEY) = match method {
        PasteMethod::CtrlV => (&[VK_CONTROL], VIRTUAL_KEY(VK_V)),
        PasteMethod::CtrlShiftV => (&[VK_CONTROL, VK_SHIFT], VIRTUAL_KEY(VK_V)),
        PasteMethod::ShiftInsert => (&[VK_SHIFT], VK_INSERT),
    };
    let ev = |vk: VIRTUAL_KEY, up: bool| {
        let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) } as u16;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: INJECTED_MARK,
                },
            },
        }
    };
    let size = std::mem::size_of::<INPUT>() as i32;
    let down: Vec<INPUT> = mods.iter().map(|m| ev(*m, false)).chain(std::iter::once(ev(key, false))).collect();
    let sent = unsafe { SendInput(&down, size) } as usize;
    hold();
    let up: Vec<INPUT> = std::iter::once(ev(key, true)).chain(mods.iter().rev().map(|m| ev(*m, true))).collect();
    let sent_up = unsafe { SendInput(&up, size) } as usize;
    if sent != down.len() || sent_up != up.len() {
        // 0 — ввод заблокирован другим потоком (BlockInput). Блокировку UIPI (окно с правами выше)
        // так не увидеть: документация SendInput — ни код возврата, ни GetLastError её не показывают;
        // её видно по отсутствию чтения буфера (on_paste в контроллере)
        return Err("Windows не приняла нажатие Ctrl+V: ввод заблокирован → «вставить последнее» повторит вставку".into());
    }
    Ok(())
}

/// Вставить текст в окно с фокусом. Возвращается сразу; итог — в `done` (из потока вставки).
pub fn paste(text: String, method: PasteMethod, restore: bool, done: impl FnOnce(PasteReport) + Send + 'static) {
    start(text, method, restore, None, false, Box::new(done));
}

/// Вставка на взводе: снимок и публикация буфера сейчас, Ctrl+V — по `Armed::fire` (окно транзакции
/// приходит вехой `Ready`). Без команды за 5 с — отмена, буфер возвращается. Вехи — в `watch`.
pub fn paste_armed(
    text: String,
    method: PasteMethod,
    restore: bool,
    watch: Sender<Milestone>,
    done: impl FnOnce(PasteReport) + Send + 'static,
) {
    start(text, method, restore, Some(watch), true, Box::new(done));
}

fn start(text: String, method: PasteMethod, restore: bool, watch: Option<Sender<Milestone>>, armed: bool, done: Done) {
    let shared = Arc::new(Shared {
        state: Mutex::new(TxState::new()),
        text: wide(&text),
        snapshot: Mutex::new(Vec::new()),
        saved_bitmap: Mutex::new(None),
        sequence: Mutex::new(0),
        restore,
        // вехам нужны квитанции чтения — текст тогда тоже «обещанием»
        wait_reads: restore || watch.is_some(),
        armed,
        method,
        watch,
        first_read: (Mutex::new(false), Condvar::new()),
        started: Instant::now(),
        chord_ms: Mutex::new(0),
        done: Mutex::new(Some(done)),
    });
    let spawned = std::thread::Builder::new().name("verba-paste".into()).spawn({
        let shared = shared.clone();
        move || unsafe { pump(shared) }
    });
    if let Err(e) = spawned {
        report(&shared, false, Some(format!("поток вставки не запустился: {e}")));
    }
}

unsafe fn pump(shared: Arc<Shared>) {
    // модификаторы человека (Ctrl+Alt+V «вставить последнее») — дождаться отпускания
    let wait_until = Instant::now() + MODIFIERS_WAIT;
    while user_modifiers_held() && Instant::now() < wait_until {
        std::thread::sleep(Duration::from_millis(10));
    }
    flush_pending();
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => return report(&shared, false, Some(e.to_string())),
    };
    ensure_class(hinst);
    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("VerbaPasteTx"),
        w!(""),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(hinst),
        None,
    ) {
        Ok(h) => h,
        Err(e) => return report(&shared, false, Some(format!("окно вставки: {e}"))),
    };
    let raw = Arc::into_raw(shared.clone());
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
    let teardown = |hwnd: HWND| {
        let _ = DestroyWindow(hwnd);
        drop(Arc::from_raw(raw));
    };

    if shared.restore {
        if let Err(e) = snapshot_clipboard(hwnd, &shared) {
            teardown(hwnd);
            return report(&shared, false, Some(format!("{e} → «вставить последнее» повторит вставку")));
        }
    }
    let sequence = match publish(hwnd, &shared) {
        Ok(s) => s,
        Err(e) => {
            if shared.restore {
                restore_snapshot(&shared);
            }
            teardown(hwnd);
            return report(&shared, false, Some(format!("{e} → «вставить последнее» повторит вставку")));
        }
    };
    *shared.sequence.lock().unwrap_or_else(|p| p.into_inner()) = sequence;
    if let Ok(mut slot) = PENDING.lock() {
        *slot = Some(shared.clone());
    }
    if shared.armed {
        if let Ok(mut st) = shared.state.lock() {
            st.armed = true;
            st.published_at = Instant::now();
        }
        shared.note(Milestone::Ready(hwnd.0 as isize));
    } else {
        start_chord(&shared);
    }
    if !shared.wait_reads {
        // текст остаётся в буфере целиком (не обещанием): ждать квитанций незачем
        std::thread::sleep(Duration::from_millis(CHORD_HOLD_MS + 50));
        teardown(hwnd);
        if let Ok(mut slot) = PENDING.lock() {
            *slot = None;
        }
        return report(&shared, false, None);
    }
    SetTimer(Some(hwnd), TIMER_ID, TIMER_INTERVAL_MS, None);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        DispatchMessageW(&msg);
    }
    let _ = KillTimer(Some(hwnd), TIMER_ID);
    // без возврата буфера невыполненное обещание отдаётся текстом здесь (WM_RENDERALLFORMATS)
    teardown(hwnd);
}

/// Текст из буфера обмена (для «Ключ OpenRouter из буфера», «Прокси из буфера»).
pub fn clipboard_text() -> Option<String> {
    unsafe {
        if !open_clipboard(None) {
            return None;
        }
        let text = (|| {
            let h = GetClipboardData(u32::from(CF_UNICODETEXT.0)).ok()?;
            let hg = HGLOBAL(h.0);
            let ptr = GlobalLock(hg) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let len = GlobalSize(hg) / 2;
            let slice = std::slice::from_raw_parts(ptr, len);
            let end = slice.iter().position(|c| *c == 0).unwrap_or(len);
            let s = String::from_utf16_lossy(&slice[..end]);
            let _ = GlobalUnlock(hg);
            Some(s)
        })();
        let _ = CloseClipboard();
        text
    }
}

/// Положить текст в буфер обычным копированием (пункт «Последние диктовки»).
pub fn copy_text(text: &str) -> bool {
    unsafe {
        if !open_clipboard(None) {
            return false;
        }
        let ok = EmptyClipboard().is_ok();
        if ok {
            render_text(&wide(text));
        }
        let _ = CloseClipboard();
        ok
    }
}

/// Очистить буфер, если в нём всё ещё `text` (ключ, скопированный ради «из буфера», там не остаётся).
pub fn clear_clipboard_if(text: &str) {
    if clipboard_text().as_deref().map(str::trim) == Some(text) {
        unsafe {
            if open_clipboard(None) {
                let _ = EmptyClipboard();
                let _ = CloseClipboard();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_reads_to_go_quiet() {
        let now = Instant::now();
        let mut s = TxState::new();
        s.published_at = now - Duration::from_millis(300);
        s.injected_at = Some(now - Duration::from_millis(250));
        assert!(!should_finish(&s, now), "квитанций нет, 8 с не прошло — ждём");
        s.receipts.push(now - Duration::from_millis(50));
        assert!(!should_finish(&s, now), "читают — ждём");
        assert!(should_finish(&s, now + QUIET_PERIOD), "стихло — возвращаем буфер");
    }

    #[test]
    fn reads_before_chord_do_not_count() {
        let now = Instant::now();
        let mut s = TxState::new();
        s.published_at = now - Duration::from_millis(300);
        s.receipts.push(now - Duration::from_millis(280));
        s.injected_at = Some(now - Duration::from_millis(250));
        assert!(!should_finish(&s, now), "менеджер буфера прочитал до Ctrl+V — это не вставка");
    }

    #[test]
    fn armed_waits_for_command_then_gives_clipboard_back() {
        let now = Instant::now();
        let mut s = TxState::new();
        s.armed = true;
        s.published_at = now - Duration::from_millis(900);
        assert!(!should_finish(&s, now), "взведена, команды ещё нет — ждём");
        assert!(should_finish(&s, now + ARM_TIMEOUT), "команды нет 5 с — буфер человеку");
        s.injected_at = Some(now);
        s.published_at = now;
        s.receipts.push(now + Duration::from_millis(10));
        assert!(!should_finish(&s, now + Duration::from_millis(100)), "нажали, читают — ждём");
        assert!(should_finish(&s, now + Duration::from_millis(10) + QUIET_PERIOD));
    }

    #[test]
    fn failed_chord_restores_fast_and_timeout_caps_wait() {
        let now = Instant::now();
        let mut s = TxState::new();
        s.published_at = now - FAILED_INJECTION_TIMEOUT;
        s.injection_failed = true;
        assert!(should_finish(&s, now));
        let mut t = TxState::new();
        t.published_at = now - RESTORE_TIMEOUT;
        t.injected_at = Some(now - RESTORE_TIMEOUT);
        assert!(should_finish(&t, now));
    }
}
