//! Настройки — `%APPDATA%\Verba\config.json`. Ключ OpenRouter и пароль прокси здесь не лежат:
//! они в Windows Credential Manager (`secrets.rs`).
//!
//! Файл правит человек: незнакомые поля игнорируются, пропущенные берутся по умолчанию,
//! битый файл не роняет программу — остаётся прежняя настройка и слова «что не так → что сделать».

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::hotkey::binding::Binding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyMode {
    /// держать: запись идёт, пока сочетание нажато
    Hold,
    /// нажал-включил: первое нажатие начинает, второе заканчивает
    Toggle,
    /// гибрид: короткое нажатие фиксирует запись (как toggle), долгое — держать
    Hybrid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub dictate: String,
    pub mode: HotkeyMode,
    /// гибрид: нажатие короче — запись остаётся включённой, длиннее — отпускание её заканчивает
    pub hybrid_hold_ms: u64,
    pub cancel: String,
    pub pause: String,
    pub paste_last: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            dictate: "ctrl+space".into(),
            mode: HotkeyMode::Hold,
            hybrid_hold_ms: 300,
            cancel: "esc".into(),
            // Ctrl+Alt+буква: игры и система такие сочетания почти не занимают
            pause: "ctrl+alt+p".into(),
            paste_last: "ctrl+alt+v".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    #[default]
    OpenRouter,
    /// ChatGPT по входу Codex (auth.json Codex, подписка ChatGPT)
    Codex,
}

impl Engine {
    /// Имя в трее и логе.
    pub fn label(self) -> &'static str {
        match self {
            Engine::OpenRouter => "OpenRouter",
            Engine::Codex => "ChatGPT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenRouter {
    /// модели в меню трея
    pub models: Vec<String>,
    /// активная модель
    pub model: String,
    /// адрес сервиса; меняют только тесты и отладка
    pub base_url: String,
}

impl Default for OpenRouter {
    fn default() -> Self {
        Self {
            models: vec![
                "openai/gpt-transcribe".into(),
                "openai/gpt-4o-mini-transcribe".into(),
                "openai/whisper-large-v3-turbo".into(),
                "openai/whisper-large-v3".into(),
            ],
            model: "openai/gpt-transcribe".into(),
            base_url: "https://openrouter.ai".into(),
        }
    }
}

/// Движок «ChatGPT по входу Codex».
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Codex {
    /// адрес ChatGPT; меняют только тесты и отладка
    pub base_url: String,
    /// путь к codex (codex.cmd или codex.exe), если его нет на PATH; null — искать самой
    pub program: Option<String>,
}

impl Default for Codex {
    fn default() -> Self {
        Self { base_url: "https://chatgpt.com".into(), program: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    /// вид не знаем: при старте пробуем HTTP, SOCKS5 и SOCKS4 сразу и берём ответивший
    Auto,
    Http,
    Https,
    Socks4,
    Socks5,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// логин; пароль — в Credential Manager
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Microphone {
    /// имя устройства как в Windows; null — устройство по умолчанию
    pub device: Option<String>,
    /// держать микрофон открытым: ни одно слово не теряется (есть пре-буфер), но значок
    /// микрофона Windows горит всегда и звук обрабатывается постоянно
    pub keep_open: bool,
    /// при keep_open: сколько звука до нажатия хоткея попадает в запись
    pub prebuffer_ms: u32,
}

impl Default for Microphone {
    fn default() -> Self {
        Self { device: None, keep_open: false, prebuffer_ms: 300 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recording {
    /// запись длиннее останавливается и уходит на распознавание
    pub max_seconds: u32,
    /// после отпускания хоткея запись идёт ещё столько: не режет последний слог
    pub tail_ms: u32,
    /// VAD отрезает тишину в начале и в конце; запись без речи не отправляется
    pub trim_silence: bool,
}

impl Default for Recording {
    fn default() -> Self {
        Self { max_seconds: 300, tail_ms: 100, trim_silence: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteMethod {
    CtrlV,
    ShiftInsert,
    CtrlShiftV,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Paste {
    pub method: PasteMethod,
    /// пробел после текста: следующая диктовка не слипается с этой
    pub trailing_space: bool,
    /// вернуть прежнее содержимое буфера обмена после вставки
    pub restore_clipboard: bool,
    /// вставлять в поле, где был курсор при нажатии хоткея, даже если фокус потом ушёл в другое
    /// окно (`docs/focus-restore.md`); выключено — вставка туда, где фокус сейчас
    pub remember_field: bool,
    /// exe игр (`game.exe`, как в Диспетчере задач): фокус в них не возвращается — Ctrl+V в игру
    /// с закрытым чатом стал бы нажатиями игровых клавиш. Полноэкранное окно — игра и без списка
    pub games: Vec<String>,
}

impl Default for Paste {
    fn default() -> Self {
        Self {
            method: PasteMethod::CtrlV,
            trailing_space: true,
            restore_clipboard: true,
            remember_field: true,
            games: Vec::new(),
        }
    }
}

/// `Game`, ` game.EXE ` → `game.exe`: так сравнивается с именем процесса.
fn normalize_exe(name: &str) -> Option<String> {
    let n = name.trim().trim_matches('"').to_lowercase();
    let n = n.rsplit(['\\', '/']).next().unwrap_or("").to_string();
    if n.is_empty() {
        return None;
    }
    Some(if n.contains('.') { n } else { format!("{n}.exe") })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sounds {
    /// звук начала и конца записи
    pub recording: bool,
    /// звук паузы и возврата
    pub pause: bool,
    /// звук ошибки
    pub errors: bool,
}

impl Default for Sounds {
    fn default() -> Self {
        Self { recording: false, pause: true, errors: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engine: Engine,
    pub hotkeys: Hotkeys,
    pub openrouter: OpenRouter,
    pub codex: Codex,
    /// прокси для обоих движков
    pub proxy: Option<ProxyConfig>,
    pub microphone: Microphone,
    pub recording: Recording,
    pub paste: Paste,
    pub sounds: Sounds,
    /// сколько последних диктовок держать в меню трея (только в памяти)
    pub history_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: Engine::OpenRouter,
            hotkeys: Hotkeys::default(),
            openrouter: OpenRouter::default(),
            codex: Codex::default(),
            proxy: None,
            microphone: Microphone::default(),
            recording: Recording::default(),
            paste: Paste::default(),
            sounds: Sounds::default(),
            history_size: 10,
        }
    }
}

/// Сочетания, уже разобранные: хук получает их числами.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bindings {
    pub dictate: Binding,
    pub cancel: Binding,
    pub pause: Binding,
    pub paste_last: Binding,
}

impl Config {
    /// Разбор и проверка того, что serde проверить не может. Ошибка — слова для человека.
    pub fn validate(&mut self) -> Result<Bindings, String> {
        let parse = |name: &str, s: &str| {
            Binding::parse(s).map_err(|e| format!("hotkeys.{name} = \"{s}\": {e} → исправь config.json"))
        };
        let b = Bindings {
            dictate: parse("dictate", &self.hotkeys.dictate)?,
            cancel: parse("cancel", &self.hotkeys.cancel)?,
            pause: parse("pause", &self.hotkeys.pause)?,
            paste_last: parse("paste_last", &self.hotkeys.paste_last)?,
        };
        let all = [("dictate", b.dictate), ("pause", b.pause), ("paste_last", b.paste_last)];
        for (i, (na, ba)) in all.iter().enumerate() {
            for (nb, bb) in all.iter().skip(i + 1) {
                if ba == bb {
                    return Err(format!("hotkeys.{na} и hotkeys.{nb} — одно сочетание → сделай их разными в config.json"));
                }
            }
        }
        self.recording.max_seconds = self.recording.max_seconds.clamp(5, 1800);
        self.recording.tail_ms = self.recording.tail_ms.min(1000);
        self.microphone.prebuffer_ms = self.microphone.prebuffer_ms.min(2000);
        self.hotkeys.hybrid_hold_ms = self.hotkeys.hybrid_hold_ms.clamp(100, 2000);
        self.history_size = self.history_size.clamp(1, 50);
        let mut games: Vec<String> = Vec::new();
        for g in self.paste.games.iter().filter_map(|g| normalize_exe(g)) {
            if !games.contains(&g) {
                games.push(g);
            }
        }
        self.paste.games = games;
        self.openrouter.models.retain(|m| !m.trim().is_empty());
        if self.openrouter.model.trim().is_empty() {
            self.openrouter.model = self.openrouter.models.first().cloned().unwrap_or_else(|| OpenRouter::default().model);
        }
        if !self.openrouter.models.contains(&self.openrouter.model) {
            self.openrouter.models.insert(0, self.openrouter.model.clone());
        }
        let base = self.openrouter.base_url.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(format!("openrouter.base_url = \"{base}\" — не адрес → верни \"https://openrouter.ai\""));
        }
        self.openrouter.base_url = base;
        let base = self.codex.base_url.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(format!("codex.base_url = \"{base}\" — не адрес → верни \"https://chatgpt.com\""));
        }
        self.codex.base_url = base;
        self.codex.program = self.codex.program.take().map(|p| p.trim().trim_matches('"').to_string()).filter(|p| !p.is_empty());
        Ok(b)
    }
}

/// Где Verba держит свои файлы.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `%APPDATA%\Verba` — настройки
    pub config_dir: PathBuf,
    /// `%LOCALAPPDATA%\Verba` — лог и несохранённая запись
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        // VERBA_HOME — одна папка на всё (тесты, переносная копия)
        if let Some(home) = std::env::var_os("VERBA_HOME") {
            let p = PathBuf::from(home);
            return Self { config_dir: p.clone(), data_dir: p };
        }
        let roaming = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| roaming.clone());
        Self { config_dir: roaming.join("Verba"), data_dir: local.join("Verba") }
    }
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
    pub fn log_file(&self) -> PathBuf {
        self.log_dir().join("verba.log")
    }
    /// запись, которая ещё не распознана: переживает перезапуск
    pub fn pending_file(&self) -> PathBuf {
        self.data_dir.join("pending.wav")
    }
}

/// Прочитанный конфиг и время изменения файла (по нему видно, что файл правили).
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub bindings: Bindings,
    pub modified: Option<SystemTime>,
}

pub fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Читает конфиг; файла нет — пишет настройки по умолчанию. Ошибка — слова для человека.
pub fn load(path: &Path) -> Result<Loaded, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut config = Config::default();
            let bindings = config.validate()?;
            save(path, &config).map_err(|e| format!("не записать {}: {e} → проверь права на папку", path.display()))?;
            return Ok(Loaded { config, bindings, modified: modified(path) });
        }
        Err(e) => return Err(format!("не прочитать {}: {e}", path.display())),
    };
    let mut config: Config = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("config.json не читается: {e} → исправь файл или удали его — Verba создаст новый"))?;
    let bindings = config.validate()?;
    Ok(Loaded { config, bindings, modified: modified(path) })
}

/// Пишет атомарно: временный файл и замена — читатель не увидит половину файла.
pub fn save(path: &Path, config: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(config).map_err(std::io::Error::other)?;
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let mut c = Config::default();
        let b = c.validate().unwrap();
        assert_eq!(b.dictate, Binding::parse("ctrl+space").unwrap());
        assert_eq!(c.openrouter.model, "openai/gpt-transcribe");
    }

    #[test]
    fn partial_file_takes_defaults() {
        let mut c: Config = serde_json::from_str(r#"{"hotkeys": {"mode": "hybrid"}, "unknown": 1}"#).unwrap();
        c.validate().unwrap();
        assert_eq!(c.hotkeys.mode, HotkeyMode::Hybrid);
        assert_eq!(c.hotkeys.dictate, "ctrl+space");
        assert_eq!(c.recording.max_seconds, 300);
    }

    #[test]
    fn same_hotkey_twice_is_refused() {
        let mut c = Config::default();
        c.hotkeys.pause = "ctrl+space".into();
        let e = c.validate().unwrap_err();
        assert!(e.contains("одно сочетание"), "{e}");
    }

    #[test]
    fn bad_hotkey_names_the_field() {
        let mut c = Config::default();
        c.hotkeys.dictate = "ctrl+nope".into();
        let e = c.validate().unwrap_err();
        assert!(e.contains("hotkeys.dictate"), "{e}");
    }

    #[test]
    fn engine_codex_reads_and_old_files_stay_openrouter() {
        let mut c: Config = serde_json::from_str(r#"{"engine": "codex", "codex": {"program": " \"C:\\x\\codex.cmd\" "}}"#).unwrap();
        c.validate().unwrap();
        assert_eq!(c.engine, Engine::Codex);
        assert_eq!(c.codex.base_url, "https://chatgpt.com");
        assert_eq!(c.codex.program.as_deref(), Some("C:\\x\\codex.cmd"));
        let mut old: Config = serde_json::from_str(r#"{"engine": "open_router"}"#).unwrap();
        old.validate().unwrap();
        assert_eq!(old.engine, Engine::OpenRouter);
        assert_eq!(old.codex, Codex::default());
        let mut bad = Config::default();
        bad.codex.base_url = "chatgpt.com".into();
        assert!(bad.validate().unwrap_err().contains("codex.base_url"));
    }

    #[test]
    fn active_model_joins_the_list() {
        let mut c = Config::default();
        c.openrouter.model = "x/y".into();
        c.validate().unwrap();
        assert_eq!(c.openrouter.models[0], "x/y");
    }

    #[test]
    fn remembered_field_is_on_by_default_and_games_normalized() {
        let mut c: Config = serde_json::from_str(r#"{"paste": {"games": [" CS2.EXE ", "dota2", "", "C:\\Games\\x.exe"]}}"#).unwrap();
        c.validate().unwrap();
        assert!(c.paste.remember_field, "старый файл без поля — включено");
        assert_eq!(c.paste.games, ["cs2.exe", "dota2.exe", "x.exe"]);
        let mut off: Config = serde_json::from_str(r#"{"paste": {"remember_field": false}}"#).unwrap();
        off.validate().unwrap();
        assert!(!off.paste.remember_field);
    }

    #[test]
    fn missing_file_is_created_and_reread() {
        let dir = std::env::temp_dir().join(format!("verba-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let first = load(&path).unwrap();
        assert!(path.exists());
        let again = load(&path).unwrap();
        assert_eq!(first.config, again.config);
        std::fs::write(&path, "{ broken").unwrap();
        assert!(load(&path).unwrap_err().contains("не читается"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
