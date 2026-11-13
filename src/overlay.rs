//! Оверлей записи: плашка-«пилюля» у нижнего края экрана по центру — волна громкости и время записи,
//! «Распознаю…», ошибка словами, короткая вспышка («Пауза» / «Слушаю»).
//!
//! Образцы вида:
//! - голосовой ввод QED (`qed\ui\design\voice.md`, `qed\ui\src\components\chat\Voice.tsx` `Wave`): новый столбик
//!   справа каждые 70 мс — самый громкий отрезок с прошлого, ещё не дошедшее — точки с прозрачностью .3, время
//!   моноширинными цифрами; распознавание — волна замирает с прозрачностью .45, вместо времени «Распознаю…»;
//! - оверлей Handy (`handy-098\src-tauri\src\overlay.rs`, `src\overlay\RecordingOverlay.css`): окно внизу по центру
//!   поверх всех, без фокуса; точка состояния слева, толстые столбики 4 px с просветом 3 px.
//!
//! Окно — своё layered-окно Win32 в своём потоке (CLAUDE.md «Стек»):
//! - `WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`: не берёт фокус,
//!   мышь проходит насквозь, нет в Alt+Tab и на панели задач — текст встаёт в поле, где был курсор;
//!   показ только `SW_SHOWNOACTIVATE` и `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)`;
//! - кадр — 32-битный DIB с предумноженной альфой → `UpdateLayeredWindow`; текст GDI рисует в отдельную маску
//!   (белым по чёрному, сглаживание в оттенках серого): GDI альфу не пишет, маска смешивается в кадр как покрытие
//!   (`docs/windows.md` «Оверлей»).
//!
//! Без подвисаний (CLAUDE.md §2, §7): контроллер кладёт команду в канал и идёт дальше. Поток оверлея в простое спит
//! в `GetMessageW`; таймер идёт, только пока плашка видна и что-то меняется (волна, время, срок ошибки); кадровый
//! буфер, маски текста и шрифт живут, пока плашка видна.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DeleteDC, DeleteObject, GdiFlush, GetMonitorInfoW,
    GetTextExtentPoint32W, GetTextFaceW, GetTextMetricsW, MonitorFromPoint, MonitorFromWindow, SelectObject, SetBkMode,
    SetTextColor, TextOutW, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, FW_NORMAL, HBITMAP, HDC, HFONT, HGDIOBJ,
    LOGFONTW, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, OUT_TT_PRECIS, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW, KillTimer, PostMessageW,
    RegisterClassW, SetTimer, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow, HTTRANSPARENT,
    HWND_TOPMOST, MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WM_APP, WM_CLOSE, WM_DPICHANGED, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::audio::recorder::LEVEL;
use crate::icons::Rgb;

/// Класс окна оверлея (проверки в `examples/overlay_demo.rs` находят окно по нему).
pub const WINDOW_CLASS: PCWSTR = w!("VerbaOverlay");
/// команды в канале — разобрать
const WM_OVERLAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;
/// шаг таймера, пока что-то движется: кадр каждые ~16 мс (системный тик 15,6 мс)
const FRAME_MS: u32 = 15;

/// новый столбик волны — раз в столько (как в QED)
const TICK: Duration = Duration::from_millis(70);
const ERROR_FOR: Duration = Duration::from_secs(5);
const FLASH_FOR: Duration = Duration::from_millis(1200);
/// столбиков в памяти: шире волна не бывает при любом DPI (≈ 22 места)
const MAX_BARS: usize = 64;
/// таймер опоздал больше — поток спал (сон ПК): пропущенное не догоняем
const MAX_CATCH_UP: usize = 8;

/// появление и уход плашки — прозрачностью окна целиком, без перерисовки
const FADE_IN: Duration = Duration::from_millis(90);
const FADE_OUT: Duration = Duration::from_millis(140);
const FADE_START: f32 = 0.35;

const TRANSCRIBING: &str = "Распознаю…";

/// Красный записи и янтарный распознавания — те же, что у значка трея (`icons.rs`).
const RED: Rgb = (0xE5, 0x48, 0x4D);
const AMBER: Rgb = (0xF5, 0xA5, 0x24);

/// Где плашка: у нижнего края рабочей области (над панелью задач), у верхнего или нигде.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    #[default]
    Bottom,
    Top,
    /// оверлей не показывается
    Off,
}

#[derive(Debug, Clone, PartialEq)]
enum Cmd {
    Position(Position),
    Recording,
    Transcribing,
    Error(String),
    Flash(String),
    Hide,
}

/// Ручка оверлея. Все методы — отправка в канал и возврат: поток контроллера оверлей не ждёт.
#[derive(Clone)]
pub struct Overlay {
    tx: Sender<Cmd>,
    /// окно оверлея: сообщение будит его поток после команды
    hwnd: isize,
}

impl Overlay {
    /// Поток оверлея с окном (скрытым). Ждёт только создания окна — один раз при запуске Verba.
    pub fn spawn() -> Result<Overlay, String> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("verba-overlay".into())
            .spawn(move || run(rx, ready_tx))
            .map_err(|e| format!("поток оверлея: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(hwnd)) => Ok(Overlay { tx, hwnd }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("оверлей: окно не создалось за 5 с".into()),
        }
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_OVERLAY, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Где показывать; `Off` — не показывать (видимая плашка уходит).
    pub fn set_position(&self, p: Position) {
        self.send(Cmd::Position(p));
    }

    /// Запись началась сейчас: волна и время с нуля.
    pub fn recording(&self) {
        self.send(Cmd::Recording);
    }

    /// Запись ушла распознаваться: волна замирает, вместо времени «Распознаю…».
    pub fn transcribing(&self) {
        self.send(Cmd::Transcribing);
    }

    /// Ошибка словами «что не так → что сделать» одной строкой; держится 5 с.
    pub fn error(&self, text: &str) {
        self.send(Cmd::Error(text.to_string()));
    }

    /// Короткая вспышка на 1,2 с («Пауза», «Слушаю»); поверх записи — потом запись видна снова.
    pub fn flash(&self, text: &str) {
        self.send(Cmd::Flash(text.to_string()));
    }

    pub fn hide(&self) {
        self.send(Cmd::Hide);
    }
}

// ───────────────────────────── чистая логика: шкалы, раскладка, текст ─────────────────────────────

/// Громкость блока (RMS, доля полной шкалы) → доля высоты столбика: −60 дБ и тише → 0, −12 дБ и громче → 1.
/// Шкала в децибелах, как у QED (`qed\ui\src\voice\audio.ts` `levelOf`): тихая речь видна, громкая не упирается.
fn level_norm(rms: f32) -> f32 {
    if rms.is_nan() || rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db + 60.0) / 48.0).clamp(0.0, 1.0)
}

/// Высота столбика: тишина — точка (`min`, = ширине столбика), громко — `max`.
fn bar_height(v: f32, min: f32, max: f32) -> f32 {
    min + (max - min) * v.clamp(0.0, 1.0)
}

/// Время записи: «0:07», «1:42», «12:05».
fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// Места столбиков между `left` и `right`: сколько влезает и x первого. Последний прижат к `right`, как в QED.
fn wave_slots(left: i32, right: i32, bar: i32, gap: i32) -> (usize, i32) {
    let pitch = bar + gap;
    if bar <= 0 || pitch <= 0 || right - left < bar {
        return (0, right);
    }
    let n = (right - left + gap) / pitch;
    (n as usize, right - n * pitch + gap)
}

/// Текст одной строкой: переводы строк и табуляция — пробелы, повторы пробелов схлопнуты.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Самое длинное начало текста, что влезает в `max_w` вместе с «…»; влезает весь — как есть.
/// `measure` — ширина строки в пикселях (в окне — GDI с выбранным шрифтом).
fn fit_text(text: &str, max_w: i32, measure: impl Fn(&str) -> i32) -> String {
    let line = one_line(text);
    if measure(&line) <= max_w {
        return line;
    }
    let chars: Vec<char> = line.chars().collect();
    let cut = |n: usize| -> String {
        let mut s: String = chars[..n].iter().collect::<String>().trim_end().to_string();
        s.push('…');
        s
    };
    // ширина начала с «…» растёт с длиной — бинарный поиск наибольшего, что влезает
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&cut(mid)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    cut(lo)
}

/// Размеры в физических пикселях монитора: логические (96 dpi) × масштаб, округлённые.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Metrics {
    h: i32,
    /// ширина плашки с волной (запись, распознавание)
    w_wave: i32,
    /// предел ширины для длинной ошибки
    w_max: i32,
    /// наименьшая ширина плашки с текстом
    w_text: i32,
    pad_l: i32,
    pad_r: i32,
    /// поля вспышки: текст по центру
    pad_flash: i32,
    /// точка состояния слева
    dot: i32,
    gap: i32,
    bar: i32,
    bar_gap: i32,
    bar_max: i32,
    font: i32,
    margin_bottom: i32,
    margin_top: i32,
    /// от краёв рабочей области, когда плашка очень широкая
    side: i32,
}

impl Metrics {
    fn new(dpi: u32) -> Metrics {
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let px = |v: f32| ((v * scale).round() as i32).max(1);
        Metrics {
            // T-006: 240×44 → 180×40 — «слишком широкое»; волна 13 столбиков при времени, 7 при «Распознаю…»
            h: px(40.0),
            w_wave: px(180.0),
            w_max: px(520.0),
            w_text: px(120.0),
            pad_l: px(16.0),
            pad_r: px(18.0),
            pad_flash: px(22.0),
            dot: px(8.0),
            gap: px(10.0),
            // 3,5 и 2,5: на 100 % — 4 и 3 px, как у Handy; на 150 % — 5 и 4
            bar: px(3.5),
            bar_gap: px(2.5),
            bar_max: px(20.0),
            font: px(13.0),
            margin_bottom: px(48.0),
            margin_top: px(32.0),
            side: px(16.0),
        }
    }

    /// Ширина плашки с текстом, не шире рабочей области.
    fn cap(&self, work_w: i32) -> i32 {
        self.w_max.min(work_w - 2 * self.side).max(self.w_text.min(work_w))
    }
}

/// Левый верхний угол плашки: по центру рабочей области (без панели задач), у нижнего или верхнего края.
fn place(work: RECT, w: i32, h: i32, pos: Position, m: &Metrics) -> (i32, i32) {
    let x = work.left + (work.right - work.left - w).div_euclid(2);
    let y = match pos {
        Position::Top => work.top + m.margin_top,
        Position::Bottom | Position::Off => work.bottom - h - m.margin_bottom,
    };
    // крошечная рабочая область — не вылезать за неё
    (x, y.clamp(work.top, (work.bottom - h).max(work.top)))
}

// ───────────────────────────── чистая логика: состояния ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Hidden,
    Recording { since: Instant },
    Transcribing,
    Error { text: String, until: Instant },
}

#[derive(Clone, Debug, PartialEq)]
struct Flash {
    text: String,
    until: Instant,
}

/// Что показывает оверлей; окно и GDI тут не участвуют.
struct Model {
    position: Position,
    phase: Phase,
    /// вспышка поверх состояния; кончилась — снова видно состояние
    flash: Option<Flash>,
    /// громкость отрезков по 70 мс (0…1), новые в конце
    bars: VecDeque<f32>,
    /// сколько столбиков пришло: изменилось — кадр перерисовать
    pushed: u64,
    next_bar: Instant,
}

impl Model {
    fn new(now: Instant) -> Model {
        Model {
            position: Position::default(),
            phase: Phase::Hidden,
            flash: None,
            bars: VecDeque::with_capacity(MAX_BARS),
            pushed: 0,
            next_bar: now,
        }
    }

    fn apply(&mut self, cmd: Cmd, now: Instant) {
        match cmd {
            Cmd::Position(p) => self.position = p,
            Cmd::Recording => {
                self.phase = Phase::Recording { since: now };
                self.flash = None;
                self.bars.clear();
                self.next_bar = now + TICK;
            }
            Cmd::Transcribing => {
                // волна замирает той, что была у записи; без записи (повтор отправки) — одни точки
                if !matches!(self.phase, Phase::Recording { .. }) {
                    self.bars.clear();
                }
                self.phase = Phase::Transcribing;
                self.flash = None;
            }
            Cmd::Error(text) => {
                self.phase = Phase::Error { text: one_line(&text), until: now + ERROR_FOR };
                self.flash = None;
                self.bars.clear();
            }
            Cmd::Flash(text) => self.flash = Some(Flash { text: one_line(&text), until: now + FLASH_FOR }),
            Cmd::Hide => {
                self.phase = Phase::Hidden;
                self.flash = None;
                self.bars.clear();
            }
        }
    }

    /// Истёкшие вспышка и ошибка уходят.
    fn expire(&mut self, now: Instant) {
        if self.flash.as_ref().is_some_and(|f| now >= f.until) {
            self.flash = None;
        }
        if matches!(&self.phase, Phase::Error { until, .. } if now >= *until) {
            self.phase = Phase::Hidden;
        }
    }

    /// Пора новых столбиков (по одному на каждые 70 мс): все получают громкость, взятую один раз — `take`
    /// забирает самый громкий блок с прошлого раза. true — волна изменилась.
    fn sample(&mut self, now: Instant, take: impl FnOnce() -> f32) -> bool {
        if !matches!(self.phase, Phase::Recording { .. }) || now < self.next_bar {
            return false;
        }
        let late = (now - self.next_bar).as_millis() / TICK.as_millis();
        let mut n = 1 + late as usize;
        if n > MAX_CATCH_UP {
            n = 1;
            self.next_bar = now + TICK;
        } else {
            self.next_bar += TICK * n as u32;
        }
        let v = level_norm(take());
        for _ in 0..n {
            self.bars.push_back(v);
        }
        while self.bars.len() > MAX_BARS {
            self.bars.pop_front();
        }
        self.pushed += n as u64;
        true
    }

    /// Нужна ли плашка на экране.
    fn wants_window(&self) -> bool {
        self.position != Position::Off && (self.phase != Phase::Hidden || self.flash.is_some())
    }

    /// Идёт запись: волна и время меняются сами — нужен таймер кадров.
    fn moving(&self) -> bool {
        matches!(self.phase, Phase::Recording { .. })
    }

    /// Ближайший срок: конец вспышки или ошибки.
    fn deadline(&self) -> Option<Instant> {
        let e = match &self.phase {
            Phase::Error { until, .. } => Some(*until),
            _ => None,
        };
        match (self.flash.as_ref().map(|f| f.until), e) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Что нарисовать сейчас.
    fn body(&self, now: Instant) -> Option<Body> {
        if let Some(f) = &self.flash {
            return Some(Body::Text { text: f.text.clone(), dot: None, center: true });
        }
        match &self.phase {
            Phase::Hidden => None,
            Phase::Recording { since } => Some(Body::Wave { live: true, label: clock(now - *since) }),
            Phase::Transcribing => Some(Body::Wave { live: false, label: TRANSCRIBING.into() }),
            Phase::Error { text, .. } => Some(Body::Text { text: text.clone(), dot: Some(RED), center: false }),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Body {
    /// точка состояния, волна, справа время или «Распознаю…»
    Wave { live: bool, label: String },
    /// текст одной строкой: ошибка (с красной точкой) или вспышка (по центру)
    Text { text: String, dot: Option<Rgb>, center: bool },
}

// ───────────────────────────── чистая логика: пиксели ─────────────────────────────

/// Цвета плашки по теме.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Theme {
    plate: Rgb,
    plate_a: f32,
    /// столбики, текст ошибки и вспышки
    ink: Rgb,
    /// время и «Распознаю…»
    muted: Rgb,
}

/// Стиль «Табло» (T-006): почти чёрная, как фон окна настроек, чуть прозрачная.
const DARK: Theme = Theme { plate: (0x11, 0x11, 0x11), plate_a: 0.94, ink: (0xF2, 0xF2, 0xF2), muted: (0x98, 0x98, 0x98) };
/// Светлая — серее белого: без рамки и тени плашка иначе теряется на белой странице.
const LIGHT: Theme = Theme { plate: (0xEA, 0xEA, 0xEA), plate_a: 0.96, ink: (0x0E, 0x0E, 0x0E), muted: (0x5C, 0x5C, 0x5C) };

/// Шрифт плашки — как у окна настроек (Bahnschrift, Windows 10 1709+); нет — Segoe UI.
const FACES: [&str; 2] = ["Bahnschrift", "Segoe UI"];

fn system_theme() -> Theme {
    if system_light() {
        LIGHT
    } else {
        DARK
    }
}

/// Есть ли шрифт в Windows: GDI молча подставляет другой, поэтому сверяется имя выбранного. Таблица шрифтов GDI —
/// в памяти, диск не читается.
pub fn has_font(face: &str) -> bool {
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return false;
        }
        let mut lf = LOGFONTW {
            lfHeight: -12,
            lfCharSet: DEFAULT_CHARSET,
            lfOutPrecision: OUT_TT_PRECIS,
            lfClipPrecision: CLIP_DEFAULT_PRECIS,
            ..Default::default()
        };
        for (d, s) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
            *d = s;
        }
        let font = CreateFontIndirectW(&lf);
        let mut found = false;
        if !font.is_invalid() {
            let old = SelectObject(dc, HGDIOBJ(font.0));
            let mut name = [0u16; 64];
            let n = GetTextFaceW(dc, Some(&mut name)).max(0) as usize;
            let got = String::from_utf16_lossy(&name[..n.min(name.len())]);
            found = got.trim_end_matches('\0').eq_ignore_ascii_case(face);
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        let _ = DeleteDC(dc);
        found
    }
}

/// Светлая ли тема Windows (панель задач, «Пуск», всплывающие окна системы): плашка — системная подсказка
/// над панелью задач. `VERBA_OVERLAY_THEME=dark|light` — проверка обеих тем (`examples/overlay_demo.rs`).
fn system_light() -> bool {
    match std::env::var("VERBA_OVERLAY_THEME").as_deref() {
        Ok("dark") => return false,
        Ok("light") => return true,
        _ => {}
    }
    let mut value: u32 = 0;
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
    // нет значения (старая Windows) — тёмная
    r.is_ok() && value == 1
}

/// Капсула: прямоугольник, скруглённый по короткой стороне, в пикселях кадра.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Capsule {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Capsule {
    /// Расстояние со знаком от точки до капсулы (внутри — меньше нуля). Тот же приём, что у значков (`icons.rs`):
    /// отрезок между центрами скруглений, расширенный на радиус; у капсулы одна полуось отрезка нулевая.
    fn dist(&self, px: f32, py: f32) -> f32 {
        let r = self.w.min(self.h) / 2.0;
        let (cx, cy) = (self.x + self.w / 2.0, self.y + self.h / 2.0);
        let dx = ((px - cx).abs() - (self.w / 2.0 - r)).max(0.0);
        let dy = ((py - cy).abs() - (self.h / 2.0 - r)).max(0.0);
        (dx * dx + dy * dy).sqrt() - r
    }
}

/// Покрытие пикселя по расстоянию: 1 внутри, 0 снаружи, сглаженный край в пиксель.
fn cover(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// Покрытие текста белым по чёрному от GDI (0…255), строки сверху вниз.
struct Mask {
    w: i32,
    h: i32,
    cov: Vec<u8>,
    /// ширина самого текста, без запаса на сглаживание по краям
    text_w: i32,
    /// от верха маски до базовой линии
    ascent: i32,
    /// кегль: высота строки без внутреннего интерлиньяжа
    em: i32,
}

/// запас маски слева и справа: сглаживание выступает за ширину текста
const OVERHANG: i32 = 2;

impl Mask {
    /// Верх маски, при котором строка встаёт по центру плашки высотой `h`: середина между базовой линией и
    /// высотой прописных (~0,7 кегля у Segoe UI) — на высоте `h / 2`.
    fn top_in(&self, h: i32) -> i32 {
        (h as f32 / 2.0 + 0.35 * self.em as f32 - self.ascent as f32).round() as i32
    }
}

/// Кадр: BGRA с предумноженной альфой, строки сверху вниз — то, что ждёт `UpdateLayeredWindow`.
struct Frame<'a> {
    px: &'a mut [u8],
    w: i32,
    h: i32,
}

impl Frame<'_> {
    /// Цвет `c` с прозрачностью `a` поверх пикселя (операция «над» для предумноженной альфы).
    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let k = 1.0 - a;
        let i = ((y * self.w + x) * 4) as usize;
        let d = &mut self.px[i..i + 4];
        d[0] = (f32::from(c.2) * a + f32::from(d[0]) * k).round() as u8;
        d[1] = (f32::from(c.1) * a + f32::from(d[1]) * k).round() as u8;
        d[2] = (f32::from(c.0) * a + f32::from(d[2]) * k).round() as u8;
        d[3] = (255.0 * a + f32::from(d[3]) * k).round() as u8;
    }

    /// Капсула со сглаженным краем; считаются только пиксели её рамки.
    fn capsule(&mut self, s: Capsule, c: Rgb, a: f32) {
        let x0 = (s.x.floor() as i32).max(0);
        let x1 = ((s.x + s.w).ceil() as i32).min(self.w);
        let y0 = (s.y.floor() as i32).max(0);
        let y1 = ((s.y + s.h).ceil() as i32).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let k = cover(s.dist(x as f32 + 0.5, y as f32 + 0.5));
                if k > 0.0 {
                    self.blend(x, y, c, a * k);
                }
            }
        }
    }

    /// Текст цветом `c`: покрытие маски × `a`; (`x`, `y`) — левый верхний угол маски.
    fn text(&mut self, m: &Mask, x: i32, y: i32, c: Rgb, a: f32) {
        for my in 0..m.h {
            for mx in 0..m.w {
                let v = m.cov[(my * m.w + mx) as usize];
                if v > 0 {
                    self.blend(x + mx, y + my, c, a * f32::from(v) / 255.0);
                }
            }
        }
    }
}

/// Плашка без содержимого: капсула во всю высоту.
fn plate(w: i32, h: i32, t: &Theme) -> Vec<u8> {
    let mut px = vec![0u8; (w.max(0) * h.max(0) * 4) as usize];
    let mut f = Frame { px: &mut px, w, h };
    f.capsule(Capsule { x: 0.0, y: 0.0, w: w as f32, h: h as f32 }, t.plate, t.plate_a);
    px
}

/// Волна: столбики — громкость отрезков (новые справа), места, до которых запись не дошла, — точки .3;
/// распознавание — всё замирает на .45.
fn draw_wave(f: &mut Frame, bars: &VecDeque<f32>, left: i32, right: i32, m: &Metrics, ink: Rgb, live: bool) {
    let (n, x0) = wave_slots(left, right, m.bar, m.bar_gap);
    let cy = f.h as f32 / 2.0;
    let (bw, min, max) = (m.bar as f32, m.bar as f32, m.bar_max as f32);
    let alpha = if live { 0.9 } else { 0.45 };
    for i in 0..n {
        let x = (x0 + i as i32 * (m.bar + m.bar_gap)) as f32;
        let (h, a) = match (bars.len() + i).checked_sub(n).and_then(|j| bars.get(j)) {
            Some(v) => (bar_height(*v, min, max), alpha),
            None => (min, 0.3),
        };
        f.capsule(Capsule { x, y: cy - h / 2.0, w: bw, h }, ink, a);
    }
}

// ───────────────────────────── Win32: текст, кадр, окно ─────────────────────────────

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Строка для `BITMAPINFO` 32-битного DIB сверху вниз.
fn dib_info(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Шрифт и контекст для измерения и рисования текста; живёт, пока плашка видна.
struct TextDc {
    dc: HDC,
    font: HFONT,
    old_font: HGDIOBJ,
    tm: TEXTMETRICW,
    /// ширина самой широкой цифры: время пишется цифрами одной ширины
    digit_w: i32,
}

static FONT_LOGGED: AtomicBool = AtomicBool::new(false);

impl TextDc {
    /// Первый из `FACES`, который есть (последний — без проверки); сглаживание в оттенках серого, без ClearType:
    /// цветная подпиксельная кайма на прозрачной плашке видна.
    fn new(px: i32) -> Option<TextDc> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let mut chosen = None;
            for (i, face) in FACES.iter().enumerate() {
                let mut lf = LOGFONTW {
                    lfHeight: -px,
                    lfWeight: FW_NORMAL.0 as i32,
                    lfCharSet: DEFAULT_CHARSET,
                    lfOutPrecision: OUT_TT_PRECIS,
                    lfClipPrecision: CLIP_DEFAULT_PRECIS,
                    lfQuality: ANTIALIASED_QUALITY,
                    ..Default::default()
                };
                for (d, s) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
                    *d = s;
                }
                let font = CreateFontIndirectW(&lf);
                if font.is_invalid() {
                    continue;
                }
                let old = SelectObject(dc, HGDIOBJ(font.0));
                // GDI молча подставляет другой шрифт, если такого нет: сверить имя выбранного
                let mut name = [0u16; 64];
                let n = GetTextFaceW(dc, Some(&mut name)).max(0) as usize;
                let got = String::from_utf16_lossy(&name[..n.min(name.len())]).trim_end_matches('\0').to_string();
                if got.eq_ignore_ascii_case(face) || i + 1 == FACES.len() {
                    if !FONT_LOGGED.swap(true, Ordering::Relaxed) {
                        crate::info!("оверлей: шрифт {got}, {px} px");
                    }
                    chosen = Some((font, old));
                    break;
                }
                SelectObject(dc, old);
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
            let Some((font, old_font)) = chosen else {
                let _ = DeleteDC(dc);
                return None;
            };
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(dc, &mut tm);
            let mut t = TextDc { dc, font, old_font, tm, digit_w: 0 };
            t.digit_w = ('0'..='9').map(|c| t.width(c.encode_utf8(&mut [0; 4]))).max().unwrap_or(0);
            Some(t)
        }
    }

    fn width(&self, s: &str) -> i32 {
        let w = wide(s);
        if w.is_empty() {
            return 0;
        }
        let mut size = SIZE::default();
        unsafe {
            let _ = GetTextExtentPoint32W(self.dc, &w, &mut size);
        }
        size.cx
    }

    /// Ширина строки; `tabular` — цифры одной ширины (время не дрожит, когда меняются цифры).
    fn measure(&self, s: &str, tabular: bool) -> i32 {
        if !tabular {
            return self.width(s);
        }
        s.chars().map(|c| if c.is_ascii_digit() { self.digit_w } else { self.width(c.encode_utf8(&mut [0; 4])) }).sum()
    }

    /// Маска текста: GDI рисует белым по чёрному в свой DIB, покрытие — яркость пикселя.
    fn mask(&self, s: &str, tabular: bool) -> Option<Mask> {
        let text_w = self.measure(s, tabular);
        let (w, h) = (text_w + 2 * OVERHANG, self.tm.tmHeight.max(1));
        unsafe {
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(None, &dib_info(w, h), DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            if bits.is_null() {
                let _ = DeleteObject(HGDIOBJ(bmp.0));
                return None;
            }
            let len = (w * h * 4) as usize;
            std::slice::from_raw_parts_mut(bits as *mut u8, len).fill(0);
            let old = SelectObject(self.dc, HGDIOBJ(bmp.0));
            SetBkMode(self.dc, TRANSPARENT);
            SetTextColor(self.dc, COLORREF(0x00FF_FFFF));
            if tabular {
                // по символу: узкая цифра — посередине своей клетки, как у моноширинных цифр шрифта
                let mut x = OVERHANG;
                for c in s.chars() {
                    let mut buf = [0u16; 2];
                    let units = c.encode_utf16(&mut buf);
                    let own = self.width(c.encode_utf8(&mut [0; 4]));
                    let cell = if c.is_ascii_digit() { self.digit_w } else { own };
                    let _ = TextOutW(self.dc, x + (cell - own) / 2, 0, units);
                    x += cell;
                }
            } else {
                let _ = TextOutW(self.dc, OVERHANG, 0, &wide(s));
            }
            let _ = GdiFlush();
            let src = std::slice::from_raw_parts(bits as *const u8, len);
            let cov = src.as_chunks::<4>().0.iter().map(|p| p[0].max(p[1]).max(p[2])).collect();
            SelectObject(self.dc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            Some(Mask {
                w,
                h,
                cov,
                text_w,
                ascent: self.tm.tmAscent,
                em: self.tm.tmHeight - self.tm.tmInternalLeading,
            })
        }
    }
}

impl Drop for TextDc {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old_font);
            let _ = DeleteObject(HGDIOBJ(self.font.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// DIB кадра, выбранный в свой контекст, — источник для `UpdateLayeredWindow`.
struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Surface {
    fn new(w: i32, h: i32) -> Option<Surface> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let Ok(bmp) = CreateDIBSection(None, &dib_info(w, h), DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return None;
            };
            if bits.is_null() {
                let _ = DeleteObject(HGDIOBJ(bmp.0));
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            Some(Surface { dc, bmp, old, bits: bits as *mut u8, w, h })
        }
    }

    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: DIB w×h×4 байт живёт, пока жив `bmp`; доступ только из потока оверлея
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Кадр на экран: место, размер, пиксели с предумноженной альфой и прозрачность окна целиком.
/// Всегда с `hdcSrc`: вызов без него документация разрешает только с `dwFlags = 0` (`docs/windows.md`).
fn present(hwnd: HWND, s: &Surface, (x, y): (i32, i32), alpha: f32) -> windows::core::Result<()> {
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&POINT { x, y }),
            Some(&SIZE { cx: s.w, cy: s.h }),
            Some(s.dc),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }
}

/// Монитор окна на переднем плане (там человек работает), иначе основной: рабочая область и DPI.
fn monitor_now() -> Option<(RECT, u32)> {
    unsafe {
        let fg = GetForegroundWindow();
        let mon = if fg.is_invalid() {
            MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY)
        };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return None;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_err() {
            dx = 96;
        }
        Some((mi.rcWork, dx))
    }
}

/// Что нарисовано в последнем кадре: совпало — кадр не перерисовывать.
#[derive(Clone, Debug, PartialEq)]
struct FrameKey {
    body: Body,
    pushed: u64,
}

/// Всё, что живёт, пока плашка на экране.
struct Shown {
    work: RECT,
    m: Metrics,
    theme: Theme,
    text: TextDc,
    surface: Option<Surface>,
    /// плашка без содержимого под текущий размер
    bg: Vec<u8>,
    /// маски текста: (строка, цифры одной ширины, маска); строк немного — время, «Распознаю…», ошибка, вспышка
    masks: Vec<(String, bool, Mask)>,
    pos: (i32, i32),
    key: Option<FrameKey>,
}

impl Shown {
    fn open() -> Option<Shown> {
        let (work, dpi) = monitor_now()?;
        let m = Metrics::new(dpi);
        let text = TextDc::new(m.font)?;
        Some(Shown {
            work,
            m,
            theme: system_theme(),
            text,
            surface: None,
            bg: Vec::new(),
            masks: Vec::new(),
            pos: (0, 0),
            key: None,
        })
    }

    fn mask(&mut self, s: &str, tabular: bool) -> Option<usize> {
        if let Some(i) = self.masks.iter().position(|(t, tab, _)| t == s && *tab == tabular) {
            return Some(i);
        }
        let m = self.text.mask(s, tabular)?;
        if self.masks.len() >= 6 {
            self.masks.remove(0);
        }
        self.masks.push((s.to_string(), tabular, m));
        Some(self.masks.len() - 1)
    }
}

struct Ctx {
    hwnd: HWND,
    rx: Receiver<Cmd>,
    model: Model,
    shown: Option<Shown>,
    /// прозрачность окна целиком: появление и уход
    alpha: f32,
    /// прозрачность, с которой окно сейчас на экране: та же — `UpdateLayeredWindow` не звать
    alpha_shown: f32,
    /// окно показано (`ShowWindow`); живёт отдельно от `shown`: ресурсы плашки могут пересоздаваться
    on_screen: bool,
    last: Instant,
    /// плашка была нужна на прошлом шаге: сейчас нужна, а тогда нет — новый показ
    wanted: bool,
    timer_ms: Option<u32>,
    /// не вышло открыть плашку (GDI) — следующая попытка не раньше
    retry_at: Option<Instant>,
}

impl Ctx {
    /// Разобрать команды, сдвинуть время, нарисовать, что изменилось, и решить, нужен ли таймер.
    fn pump(&mut self) {
        let now = Instant::now();
        // шаг прозрачности — не больше двух кадров: после простоя (таймер стоял) плашка не мигает разом
        let dt = now.saturating_duration_since(self.last).min(Duration::from_millis(2 * u64::from(FRAME_MS)));
        self.last = now;
        let (mut moved, mut new_recording) = (false, false);
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                Cmd::Recording => {
                    // громкость прошлой записи — не в новую волну
                    LEVEL.swap(0, Ordering::Relaxed);
                    new_recording = true;
                }
                Cmd::Position(p) => moved |= p != self.model.position,
                _ => {}
            }
            self.model.apply(cmd, now);
        }
        self.model.expire(now);
        self.model.sample(now, || f32::from_bits(LEVEL.swap(0, Ordering::Relaxed)));

        let want = self.model.wants_window();
        // новый показ: плашки не было (или уходила) или началась запись — монитор, DPI и тема берутся сейчас
        let fresh = want && (!self.wanted || new_recording);
        self.wanted = want;
        if want {
            if !self.ensure_shown(now, fresh) {
                self.schedule(now);
                return;
            }
            self.alpha = if fresh { self.alpha.max(FADE_START) } else { step(self.alpha, 1.0, dt, FADE_IN) };
            self.redraw(now, moved || fresh);
        } else if self.on_screen {
            self.alpha = step(self.alpha, 0.0, dt, FADE_OUT);
            if self.alpha <= 0.0 {
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                self.on_screen = false;
                self.alpha_shown = 0.0;
                // буферы, маски и шрифт — до следующего показа (память в простое, CLAUDE.md §7)
                self.shown = None;
            } else {
                self.push_alpha();
            }
        } else {
            // плашка не успела показаться (выключена, не открылась) — ресурсы не держать
            self.shown = None;
            self.alpha = 0.0;
        }
        self.schedule(now);
    }

    /// Только прозрачность окна: содержимое то же — тот же кадр с новой прозрачностью. Кадра нет (плашку
    /// пересоздавали и не вышло) — окно скрывается сразу, без ухода.
    fn push_alpha(&mut self) {
        if !self.on_screen || (self.alpha - self.alpha_shown).abs() < 1e-3 {
            return;
        }
        let ok = self
            .shown
            .as_ref()
            .and_then(|s| s.surface.as_ref().map(|f| present(self.hwnd, f, s.pos, self.alpha).is_ok()))
            .unwrap_or(false);
        if ok {
            self.alpha_shown = self.alpha;
        } else if !self.model.wants_window() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.on_screen = false;
            self.alpha = 0.0;
            self.alpha_shown = 0.0;
            self.shown = None;
        }
    }

    /// Плашка открыта; при новом показе — на мониторе, где человек сейчас. false — открыть не вышло.
    fn ensure_shown(&mut self, now: Instant, fresh: bool) -> bool {
        if let Some(s) = self.shown.as_mut() {
            if !fresh {
                return true;
            }
            match monitor_now() {
                Some((work, dpi)) if Metrics::new(dpi) == s.m => {
                    s.work = work;
                    s.theme = system_theme();
                    s.key = None;
                    s.bg.clear();
                    return true;
                }
                // другой DPI (или монитор не ответил) — шрифт и размеры заново
                _ => self.shown = None,
            }
        }
        if self.retry_at.is_some_and(|t| now < t) {
            return false;
        }
        match Shown::open() {
            Some(s) => {
                self.shown = Some(s);
                self.retry_at = None;
                true
            }
            None => {
                crate::warn!("оверлей: плашку не открыть (монитор или GDI) — попробую через секунду");
                self.retry_at = Some(now + Duration::from_secs(1));
                false
            }
        }
    }

    fn redraw(&mut self, now: Instant, relocate: bool) {
        let Some(body) = self.model.body(now) else { return };
        let position = self.model.position;
        let alpha = self.alpha;
        let Some(sh) = self.shown.as_mut() else { return };
        let m = sh.m;
        let work_w = sh.work.right - sh.work.left;

        // ширина и текст: волна — плашка постоянной ширины; текст — по ширине строки, длинная обрезается с «…»
        let (w, text_line) = match &body {
            Body::Wave { .. } => (m.w_wave, None),
            Body::Text { text, dot, .. } => {
                let cap = m.cap(work_w);
                let lead = if dot.is_some() { m.pad_l + m.dot + m.gap } else { m.pad_flash };
                let tail = if dot.is_some() { m.pad_r } else { m.pad_flash };
                let line = fit_text(text, cap - lead - tail, |s| sh.text.measure(s, false));
                let tw = sh.text.measure(&line, false);
                ((lead + tw + tail).clamp(m.w_text.min(cap), cap), Some(line))
            }
        };
        // у записи справа время (цифры одной ширины), у распознавания — «Распознаю…»
        let label = match &body {
            Body::Wave { label, live } => Some((label.clone(), *live)),
            Body::Text { .. } => None,
        };
        let h = m.h;

        let key = FrameKey { body: body.clone(), pushed: self.model.pushed };
        let size_changed = sh.surface.as_ref().is_none_or(|s| s.w != w || s.h != h);
        if !size_changed && !relocate && sh.key.as_ref() == Some(&key) {
            // содержимое то же (между столбиками, пока время не сменилось) — разве что прозрачность
            self.push_alpha();
            return;
        }
        if size_changed {
            sh.surface = Surface::new(w, h);
            sh.bg.clear();
        }
        if sh.bg.len() != (w * h * 4) as usize {
            sh.bg = plate(w, h, &sh.theme);
        }
        let kind_changed = sh.key.as_ref().map(|k| std::mem::discriminant(&k.body)) != Some(std::mem::discriminant(&body));
        sh.pos = place(sh.work, w, h, position, &m);

        // маски до заимствования кадра; у кадра одна маска — время/«Распознаю…» или строка текста, поэтому
        // вытеснение из кэша (сдвиг индексов) второй маски не задевает
        let label_mask = match &label {
            Some((s, live)) => sh.mask(s, *live),
            None => None,
        };
        let line_mask = text_line.as_deref().and_then(|s| sh.mask(s, false));
        let theme = sh.theme;
        let Some(surface) = sh.surface.as_mut() else { return };
        let px = surface.pixels();
        px.copy_from_slice(&sh.bg);
        let mut f = Frame { px, w, h };
        let dot_at = |x: i32| Capsule { x: x as f32, y: ((h - m.dot) / 2) as f32, w: m.dot as f32, h: m.dot as f32 };
        match &body {
            Body::Wave { live, .. } => {
                f.capsule(dot_at(m.pad_l), if *live { RED } else { AMBER }, 1.0);
                let mut right = w - m.pad_r;
                if let Some(i) = label_mask {
                    let mk = &sh.masks[i].2;
                    let x = w - m.pad_r - mk.text_w;
                    f.text(mk, x - OVERHANG, mk.top_in(h), theme.muted, 1.0);
                    right = x - m.gap;
                }
                draw_wave(&mut f, &self.model.bars, m.pad_l + m.dot + m.gap, right, &m, theme.ink, *live);
            }
            Body::Text { dot, center, .. } => {
                let mut x = m.pad_flash;
                if let Some(c) = dot {
                    f.capsule(dot_at(m.pad_l), *c, 1.0);
                    x = m.pad_l + m.dot + m.gap;
                }
                if let Some(i) = line_mask {
                    let mk = &sh.masks[i].2;
                    if *center {
                        x = (w - mk.text_w) / 2;
                    }
                    f.text(mk, x - OVERHANG, mk.top_in(h), theme.ink, 1.0);
                }
            }
        }

        if let Err(e) = present(self.hwnd, surface, sh.pos, alpha) {
            crate::warn!("оверлей: кадр не встал ({e})");
            return;
        }
        self.alpha_shown = alpha;
        unsafe {
            if !self.on_screen {
                // без активации: фокус остаётся в поле, где был курсор
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.on_screen = true;
            }
            if kind_changed {
                // поверх всех, в том числе панели задач — каждый раз, когда меняется вид
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
        sh.key = Some(key);
    }

    /// Таймер: кадры, пока идёт запись или прозрачность меняется; к сроку вспышки или ошибки; иначе — нет.
    fn schedule(&mut self, now: Instant) {
        let fading = (self.shown.is_some() || self.on_screen) && {
            let target = if self.model.wants_window() { 1.0 } else { 0.0 };
            self.alpha != target
        };
        let ms = if self.retry_at.is_some() && self.model.wants_window() {
            Some(1000)
        } else if fading || (self.model.moving() && self.model.wants_window()) {
            Some(FRAME_MS)
        } else if self.shown.is_some() || self.model.wants_window() {
            self.model.deadline().map(|d| (d.saturating_duration_since(now).as_millis() as u32).clamp(FRAME_MS, 60_000))
        } else {
            None
        };
        if ms == self.timer_ms {
            return;
        }
        unsafe {
            match ms {
                Some(v) => {
                    SetTimer(Some(self.hwnd), TIMER_ID, v, None);
                }
                None => {
                    let _ = KillTimer(Some(self.hwnd), TIMER_ID);
                }
            }
        }
        self.timer_ms = ms;
    }
}

/// Шаг прозрачности к `target` за `dt`; `full` — время от 0 до 1.
fn step(alpha: f32, target: f32, dt: Duration, full: Duration) -> f32 {
    let d = dt.as_secs_f32() / full.as_secs_f32().max(f32::EPSILON);
    if alpha < target {
        (alpha + d).min(target)
    } else {
        (alpha - d).max(target)
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // мышь — сквозь плашку, щелчок не активирует (основное — WS_EX_TRANSPARENT у layered-окна)
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // размеры под DPI монитора считает сам оверлей при каждом показе
        WM_DPICHANGED => LRESULT(0),
        // окно живёт, пока жив процесс
        WM_CLOSE => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_window() -> Result<HWND, String> {
    unsafe {
        let hinst = HINSTANCE(GetModuleHandleW(None).map_err(|e| format!("оверлей: {e}"))?.0);
        let wc = WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: hinst, lpszClassName: WINDOW_CLASS, ..Default::default() };
        // второй вызов (тесты, второй оверлей) — класс уже есть, окно создаётся по нему
        RegisterClassW(&wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("Verba"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )
        .map_err(|e| format!("окно оверлея: {e}"))
    }
}

fn run(rx: Receiver<Cmd>, ready: Sender<Result<isize, String>>) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(hwnd.0 as isize));
    let now = Instant::now();
    let mut ctx = Ctx {
        hwnd,
        rx,
        model: Model::new(now),
        shown: None,
        alpha: 0.0,
        alpha_shown: 0.0,
        on_screen: false,
        last: now,
        wanted: false,
        timer_ms: None,
        retry_at: None,
    };
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd == hwnd && (msg.message == WM_OVERLAY || (msg.message == WM_TIMER && msg.wParam.0 == TIMER_ID)) {
                ctx.pump();
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn loudness_scale_is_in_decibels() {
        assert_eq!(level_norm(0.0), 0.0);
        assert_eq!(level_norm(f32::NAN), 0.0);
        assert_eq!(level_norm(-0.5), 0.0);
        assert_eq!(level_norm(0.001), 0.0, "−60 дБ — тишина");
        assert!((level_norm(10f32.powf(-36.0 / 20.0)) - 0.5).abs() < 1e-4, "−36 дБ — половина");
        assert_eq!(level_norm(0.3), 1.0, "громче −12 дБ — полный столбик");
        assert!(level_norm(0.25) > 0.99, "0,25 — это −12,04 дБ");
        assert_eq!(level_norm(1.0), 1.0);
        // тихая речь (−45 дБ) уже заметна: больше четверти высоты сверх точки
        assert!(level_norm(10f32.powf(-45.0 / 20.0)) > 0.3);
        assert_eq!(bar_height(0.0, 4.0, 22.0), 4.0);
        assert_eq!(bar_height(1.0, 4.0, 22.0), 22.0);
        assert_eq!(bar_height(7.0, 4.0, 22.0), 22.0);
    }

    #[test]
    fn level_bits_keep_order_for_fetch_max() {
        // у неотрицательных f32 порядок битов как у чисел — колбэк микрофона кладёт уровень через fetch_max
        let vals = [0.0f32, 1e-9, 0.001, 0.0011, 0.5, 0.75, 1.0, 3.0];
        for w in vals.windows(2) {
            assert!(w[0].to_bits() < w[1].to_bits(), "{w:?}");
        }
        let a = std::sync::atomic::AtomicU32::new(0);
        for v in [0.2f32, 0.7, 0.1] {
            a.fetch_max(v.to_bits(), Ordering::Relaxed);
        }
        assert_eq!(f32::from_bits(a.swap(0, Ordering::Relaxed)), 0.7);
        assert_eq!(a.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn clock_is_minutes_and_seconds() {
        assert_eq!(clock(ms(0)), "0:00");
        assert_eq!(clock(ms(7_900)), "0:07");
        assert_eq!(clock(ms(102_000)), "1:42");
        assert_eq!(clock(ms(725_000)), "12:05");
    }

    #[test]
    fn wave_slots_fill_from_the_right() {
        // 152 px, столбик 4, просвет 3: (152 + 3) / 7 = 22
        let (n, x0) = wave_slots(34, 186, 4, 3);
        assert_eq!(n, 22);
        assert_eq!(x0 + (n as i32 - 1) * 7 + 4, 186, "последний прижат к правому краю");
        assert!(x0 >= 34);
        assert_eq!(wave_slots(10, 12, 4, 3).0, 0, "не влезает ни один");
        assert_eq!(wave_slots(10, 14, 4, 3).0, 1);
        assert_eq!(wave_slots(10, 100, 0, 3).0, 0);
    }

    #[test]
    fn long_text_is_cut_with_ellipsis() {
        let measure = |s: &str| s.chars().count() as i32 * 7;
        assert_eq!(fit_text("Нет сети", 100, measure), "Нет сети");
        assert_eq!(fit_text("  две\nстроки\tвместе ", 1000, measure), "две строки вместе");
        let cut = fit_text("Сервис не ответил за 7 с → проверь сеть", 70, measure);
        assert!(cut.ends_with('…'), "{cut}");
        assert!(measure(&cut) <= 70, "{cut}");
        assert_eq!(cut, "Сервис не…", "ровно 70 px влезает");
        assert_eq!(fit_text("Сервис не ответил", 56, measure), "Сервис…", "пробел перед многоточием срезан");
        assert_eq!(fit_text("абв", 3, measure), "…");
        assert_eq!(fit_text("", 10, measure), "");
    }

    #[test]
    fn metrics_follow_dpi() {
        let m = Metrics::new(96);
        assert_eq!((m.h, m.w_wave, m.bar, m.bar_gap, m.bar_max, m.font), (40, 180, 4, 3, 20, 13));
        assert_eq!(m.dot, 8);
        let m = Metrics::new(144);
        assert_eq!((m.h, m.w_wave, m.bar, m.bar_gap), (60, 270, 5, 4));
        let m = Metrics::new(192);
        assert_eq!((m.h, m.w_wave, m.bar, m.bar_gap, m.w_max), (80, 360, 7, 5, 1040));
        assert_eq!(Metrics::new(0), Metrics::new(96));
        // плашка с текстом не шире рабочей области
        let m = Metrics::new(96);
        assert_eq!(m.cap(1920), 520);
        assert_eq!(m.cap(400), 368);
    }

    #[test]
    fn plate_sits_above_the_taskbar_centered() {
        let m = Metrics::new(96);
        // 1920×1080, панель задач 48 снизу
        let work = RECT { left: 0, top: 0, right: 1920, bottom: 1032 };
        assert_eq!(place(work, 240, 44, Position::Bottom, &m), (840, 1032 - 44 - 48));
        assert_eq!(place(work, 240, 44, Position::Top, &m), (840, 32));
        // второй монитор слева и выше, 150 %
        let m = Metrics::new(144);
        let work = RECT { left: -2560, top: -200, right: 0, bottom: 1240 - 72 };
        let (x, y) = place(work, m.w_wave, m.h, Position::Bottom, &m);
        assert_eq!(x, -2560 + (2560 - 270) / 2);
        assert_eq!(y, 1168 - 60 - 72);
        // нечётный остаток — без сдвига на полпикселя наружу
        let work = RECT { left: -1001, top: 0, right: 0, bottom: 500 };
        assert_eq!(place(work, 240, 44, Position::Bottom, &Metrics::new(96)).0, -1001 + 380);
        // крошечная рабочая область — внутри неё
        let tiny = RECT { left: 0, top: 0, right: 300, bottom: 60 };
        let (_, y) = place(tiny, 240, 44, Position::Bottom, &Metrics::new(96));
        assert_eq!(y, 0);
    }

    #[test]
    fn capsule_has_round_ends_and_soft_edge() {
        let s = Capsule { x: 0.0, y: 0.0, w: 240.0, h: 44.0 };
        assert_eq!(cover(s.dist(120.5, 22.5)), 1.0, "середина");
        assert_eq!(cover(s.dist(0.5, 0.5)), 0.0, "угол срезан скруглением");
        assert_eq!(cover(s.dist(120.5, 0.5)), 1.0, "верхний ряд середины — внутри");
        assert_eq!(cover(s.dist(120.5, -0.5)), 0.0);
        let edge = cover(s.dist(0.5, 22.0));
        assert!(edge > 0.9, "левый край на оси — почти полностью: {edge}");
        // по скруглению: точка на расстоянии ровно r от центра скругления — половина пикселя
        let (cx, cy) = (22.0f32, 22.0f32);
        let a = std::f32::consts::FRAC_PI_4;
        let half = cover(s.dist(cx - 22.0 * a.cos(), cy - 22.0 * a.sin()));
        assert!((half - 0.5).abs() < 1e-3, "{half}");
        // столбик-точка: ширина = высота → круг
        let d = Capsule { x: 10.0, y: 10.0, w: 4.0, h: 4.0 };
        assert_eq!(cover(d.dist(12.0, 12.0)), 1.0);
        assert_eq!(cover(d.dist(10.2, 10.2)), 0.0);
    }

    #[test]
    fn blending_keeps_premultiplied_alpha() {
        let mut px = vec![0u8; 4];
        let mut f = Frame { px: &mut px, w: 1, h: 1 };
        f.blend(0, 0, (200, 100, 50), 0.5);
        assert_eq!(px, [25, 50, 100, 128], "BGRA, цвет умножен на альфу");
        let mut f = Frame { px: &mut px, w: 1, h: 1 };
        f.blend(0, 0, (255, 255, 255), 1.0);
        assert_eq!(px, [255, 255, 255, 255]);
        let mut f = Frame { px: &mut px, w: 1, h: 1 };
        f.blend(0, 0, (0, 0, 0), 0.0);
        f.blend(5, 5, (0, 0, 0), 1.0);
        assert_eq!(px, [255, 255, 255, 255], "нулевая альфа и вне кадра — без изменений");
        // плашка: каждый канал не больше альфы (иначе UpdateLayeredWindow даёт пересвет)
        let bg = plate(60, 20, &DARK);
        for p in bg.as_chunks::<4>().0 {
            assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3], "{p:?}");
        }
        assert_eq!(bg[0..4], [0, 0, 0, 0], "угол прозрачный");
        let mid = ((10 * 60 + 30) * 4) as usize;
        assert_eq!(bg[mid + 3], (DARK.plate_a * 255.0).round() as u8, "середина — плашка");
    }

    #[test]
    fn wave_draws_bars_right_and_dots_left() {
        let m = Metrics::new(96);
        let (w, h) = (200, m.h);
        let mut px = vec![0u8; (w * h * 4) as usize];
        let mut f = Frame { px: &mut px, w, h };
        let bars: VecDeque<f32> = [1.0f32, 0.0].into_iter().collect();
        draw_wave(&mut f, &bars, 0, w, &m, (255, 255, 255), true);
        let alpha_at = |px: &[u8], x: i32, y: i32| px[((y * w + x) * 4 + 3) as usize];
        let (n, x0) = wave_slots(0, w, m.bar, m.bar_gap);
        let col = |i: usize| x0 + i as i32 * (m.bar + m.bar_gap) + m.bar / 2;
        let cy = h / 2;
        // предпоследнее место — громкий столбик: высоко над серединой ещё закрашено
        assert!(alpha_at(&px, col(n - 2), cy - 9) > 200);
        // последнее — тишина: точка, выше неё пусто
        assert!(alpha_at(&px, col(n - 1), cy) > 200);
        assert_eq!(alpha_at(&px, col(n - 1), cy - 6), 0);
        // левее — не дошедшее: бледная точка .3
        let dot = alpha_at(&px, col(0), cy);
        assert!(dot > 60 && dot < 90, "{dot}");
    }

    #[test]
    fn model_records_and_samples_every_70_ms() {
        let t0 = Instant::now();
        let mut m = Model::new(t0);
        assert!(!m.wants_window());
        m.apply(Cmd::Recording, t0);
        assert!(m.wants_window() && m.moving());
        assert!(!m.sample(t0 + ms(69), || 0.1));
        assert!(m.sample(t0 + ms(70), || 0.3));
        assert_eq!(m.bars.len(), 1);
        assert_eq!(m.bars[0], 1.0);
        for k in 2..=10 {
            m.sample(t0 + ms(70 * k), || 0.0);
        }
        assert_eq!(m.bars.len(), 10);
        assert_eq!(m.pushed, 10);
        // таймер опоздал на два отрезка — оба получают ту же громкость
        assert!(m.sample(t0 + ms(70 * 12 + 5), || 0.3));
        assert_eq!(m.bars.len(), 12);
        assert_eq!(&[m.bars[10], m.bars[11]], &[1.0, 1.0]);
        // поток проспал секунды (сон ПК) — один столбик, без догоняния
        assert!(m.sample(t0 + ms(70 * 12 + 5_000), || 0.0));
        assert_eq!(m.bars.len(), 13);
        // память волны ограничена
        let mut t = t0 + ms(70 * 12 + 5_000);
        for _ in 0..200 {
            t += TICK;
            m.sample(t, || 0.0);
        }
        assert_eq!(m.bars.len(), MAX_BARS);
        assert_eq!(m.body(t0 + ms(7_500)), Some(Body::Wave { live: true, label: "0:07".into() }));
    }

    #[test]
    fn transcribing_freezes_the_wave_and_hide_clears_it() {
        let t0 = Instant::now();
        let mut m = Model::new(t0);
        m.apply(Cmd::Recording, t0);
        m.sample(t0 + ms(70), || 0.1);
        m.apply(Cmd::Transcribing, t0 + ms(100));
        assert_eq!(m.bars.len(), 1, "волна записи замерла");
        assert!(!m.moving());
        assert!(!m.sample(t0 + ms(500), || 0.1), "после записи столбики не идут");
        assert_eq!(m.body(t0), Some(Body::Wave { live: false, label: TRANSCRIBING.into() }));
        assert_eq!(m.deadline(), None, "распознавание держится, пока не скажут иначе");
        m.apply(Cmd::Hide, t0 + ms(600));
        assert!(!m.wants_window());
        assert!(m.bars.is_empty());
        // повтор отправки без записи — одни точки
        m.apply(Cmd::Transcribing, t0 + ms(700));
        assert!(m.bars.is_empty() && m.wants_window());
    }

    #[test]
    fn error_and_flash_expire() {
        let t0 = Instant::now();
        let mut m = Model::new(t0);
        m.apply(Cmd::Error("нет\nсети".into()), t0);
        assert_eq!(m.body(t0), Some(Body::Text { text: "нет сети".into(), dot: Some(RED), center: false }));
        assert_eq!(m.deadline(), Some(t0 + ERROR_FOR));
        m.expire(t0 + ERROR_FOR - ms(1));
        assert!(m.wants_window());
        m.expire(t0 + ERROR_FOR);
        assert!(!m.wants_window(), "ошибка ушла через 5 с");

        m.apply(Cmd::Flash("Пауза".into()), t0);
        assert!(m.wants_window());
        m.expire(t0 + FLASH_FOR);
        assert!(!m.wants_window(), "вспышка одна — после неё скрыто");
    }

    #[test]
    fn flash_over_recording_gives_the_recording_back() {
        let t0 = Instant::now();
        let mut m = Model::new(t0);
        m.apply(Cmd::Recording, t0);
        m.apply(Cmd::Flash("Пауза".into()), t0 + ms(500));
        assert_eq!(m.body(t0 + ms(600)), Some(Body::Text { text: "Пауза".into(), dot: None, center: true }));
        // под вспышкой волна идёт дальше
        assert!(m.sample(t0 + ms(700), || 0.1));
        m.expire(t0 + ms(500) + FLASH_FOR);
        assert_eq!(m.body(t0 + ms(2_000)), Some(Body::Wave { live: true, label: "0:02".into() }));
        // новая запись гасит вспышку сразу
        m.apply(Cmd::Flash("Слушаю".into()), t0 + ms(2_100));
        m.apply(Cmd::Recording, t0 + ms(2_200));
        assert!(m.flash.is_none());
        assert_eq!(m.deadline(), None);
    }

    #[test]
    fn off_hides_without_losing_the_state() {
        let t0 = Instant::now();
        let mut m = Model::new(t0);
        m.apply(Cmd::Position(Position::Off), t0);
        m.apply(Cmd::Recording, t0);
        assert!(!m.wants_window());
        assert!(m.moving(), "запись идёт и без плашки");
        m.apply(Cmd::Position(Position::Top), t0 + ms(100));
        assert!(m.wants_window(), "включили посреди записи — плашка видна");
    }

    #[test]
    fn position_is_snake_case_in_config() {
        assert_eq!(serde_json::to_string(&Position::Bottom).unwrap(), "\"bottom\"");
        assert_eq!(serde_json::from_str::<Position>("\"off\"").unwrap(), Position::Off);
        assert_eq!(Position::default(), Position::Bottom);
    }

    #[test]
    fn fade_steps_toward_target() {
        assert_eq!(step(0.0, 1.0, ms(45), ms(90)), 0.5);
        assert_eq!(step(0.9, 1.0, ms(45), ms(90)), 1.0);
        assert_eq!(step(0.1, 0.0, ms(140), ms(140)), 0.0);
        assert_eq!(step(1.0, 1.0, ms(15), ms(90)), 1.0);
    }

    #[test]
    fn plate_text_reads_in_both_themes() {
        let lum = |c: Rgb| 0.2126 * f32::from(c.0) + 0.7152 * f32::from(c.1) + 0.0722 * f32::from(c.2);
        for t in [DARK, LIGHT] {
            // текст и время читаются на плашке: разница яркости заметная
            assert!((lum(t.ink) - lum(t.plate)).abs() > 150.0, "{t:?}");
            assert!((lum(t.muted) - lum(t.plate)).abs() > 70.0, "{t:?}");
            assert!(t.plate_a > 0.9 && t.plate_a <= 1.0);
        }
    }

    #[test]
    fn font_check_tells_real_fonts_from_substitutes() {
        assert!(has_font("Segoe UI"), "Segoe UI есть в любой Windows 10/11");
        assert!(!has_font("Нет Такого Шрифта"), "GDI подставил бы другой — проверка это видит");
        assert!(!has_font(""));
    }

    #[test]
    fn gdi_text_mask_has_coverage_and_digits_of_one_width() {
        let Some(t) = TextDc::new(13) else { panic!("шрифт не создался") };
        let m = t.mask("0:07", true).expect("маска");
        assert_eq!(m.w, m.text_w + 2 * OVERHANG);
        assert!(m.h >= 13 && m.ascent > 0 && m.em >= 12, "{} {} {}", m.h, m.ascent, m.em);
        assert!(m.cov.contains(&255), "есть полностью закрашенные пиксели");
        assert!(m.cov.iter().any(|v| *v > 0 && *v < 255), "края сглажены");
        // по краям запас пустой
        for y in 0..m.h {
            assert_eq!(m.cov[(y * m.w) as usize], 0);
        }
        assert_eq!(t.measure("1:11", true), t.measure("0:08", true), "время не дрожит");
        // строка встаёт по центру плашки: прописные и цифры около середины высоты
        let top = m.top_in(40);
        assert!(top > 3 && top + m.ascent > 20 && top + m.ascent < 30, "{top}");
    }
}
