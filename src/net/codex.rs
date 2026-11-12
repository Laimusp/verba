//! Движок «ChatGPT по входу Codex» — механизм codex-asr (github.com/Wangnov/codex-asr 0.1.2, MIT),
//! перенесённый по образцу голосового ввода QED (`src/server/voice/codex.ts`, `service.ts`
//! codexAuth / codexSend / renew):
//!
//!   `$CODEX_HOME\auth.json` (иначе `%USERPROFILE%\.codex\auth.json`) → `tokens.access_token`,
//!   `tokens.account_id` → `POST https://chatgpt.com/backend-api/transcribe`, multipart с частью
//!   `file`; заголовки `Authorization: Bearer`, `ChatGPT-Account-Id`, `originator: Codex Desktop`,
//!   его User-Agent → `{"text": "…"}`.
//!
//! Адрес не публичный: это эндпоинт самого Codex Desktop, может смениться с обновлением Codex.
//! Замеры QED (02–06.11.2026): 3 с речи → 1,1–1,6 с; по прогретому соединению 5 слов → 0,7–1,0 с;
//! 0,3 с тишины → 200 и `""` (так проверяется движок); плохой токен → 401; битый звук → 400
//! `Unable to determine audio duration`; 403 не-JSON — защита сайта или путь, а не вход.
//!
//! Вход читается из auth.json при каждой диктовке; **Verba auth.json не пишет никогда**. Токен живёт
//! 10 дней, Codex обновляет его сам. Истекает в пределах 60 с или ChatGPT ответил 401 / 403 с JSON —
//! Codex обновляет вход через свой app-server (`app_server.rs`), запись уходит ещё раз (один раз).
//! Обновление одно на всё приложение: refresh-токен одноразовый, два обновления разом — вход слетит.
//!
//! Сеть — общий механизм ядра (`Pool`): прогрев при нажатии хоткея, повтор по новому соединению.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::app_server::{find_codex, AppServer, ServerCmd};
use super::words::Service;
use super::{Answer, Failure, Hedge, Pool, Post, Route, Warm};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com";
pub const TRANSCRIBE_PATH: &str = "/backend-api/transcribe";
/// Версия Codex Desktop, которую шлёт codex-asr, когда не находит Codex.app (QED `codex.ts`).
const DESKTOP_VERSION: &str = "26.429.30905";
/// Токен истекает раньше — сначала обновление входа, потом отправка.
pub const RENEW_BEFORE: Duration = Duration::from_secs(60);
/// Ответ app-server на `getAuthStatus` с обновлением (QED `CODEX_RENEW_TIMEOUT_MS`).
pub const RENEW_TIMEOUT: Duration = Duration::from_secs(30);
/// Режимы входа auth.json с токеном ChatGPT (codex-asr: `chatgpt`, `chatgpt_auth_tokens`).
const CHATGPT_MODES: [&str; 3] = ["chatgpt", "chatgpt_auth_tokens", "chatgptAuthTokens"];

pub fn user_agent() -> String {
    format!("Codex Desktop/{DESKTOP_VERSION} (windows; x86_64)")
}

// ---------- вход ----------

/// Вход ChatGPT, как его берёт запрос. `Debug` без токена: он не должен попасть в лог.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexAuth {
    pub token: String,
    /// `ChatGPT-Account-Id`: `tokens.account_id`, иначе claim токена; None — заголовка нет
    pub account_id: Option<String>,
    /// claim `exp`, секунды Unix; None — не сказано
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("token", &"…")
            .field("account_id", &self.account_id.is_some())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    Ready(CodexAuth),
    /// файла нет или в нём нет токена: Codex на этом ПК не входил
    None,
    /// Codex вошёл по ключу API — распознаванию нужен вход ChatGPT
    ApiKey,
    /// файл есть, но не читается; причина — без содержимого файла
    Broken(String),
}

/// `$CODEX_HOME\auth.json`, иначе `%USERPROFILE%\.codex\auth.json` — где Codex держит вход.
pub fn auth_file() -> PathBuf {
    auth_file_from(std::env::var_os("CODEX_HOME"), std::env::var_os("USERPROFILE"))
}

pub fn auth_file_from(codex_home: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    let dir = match codex_home.filter(|h| !h.to_string_lossy().trim().is_empty()) {
        Some(h) => PathBuf::from(h.to_string_lossy().trim()),
        None => PathBuf::from(home.unwrap_or_default()).join(".codex"),
    };
    dir.join("auth.json")
}

/// Полезная нагрузка JWT; None — не JWT. Подпись не проверяется: её проверяет ChatGPT, Verba только
/// читает срок и аккаунт.
pub fn jwt_claims(token: &str) -> Option<serde_json::Map<String, Value>> {
    let part = token.split('.').nth(1)?;
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    match serde_json::from_slice(&engine.decode(part).ok()?).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Токен (из auth.json или от обновления) → вход; `fallback` — аккаунт, если у токена его нет.
pub fn auth_of_token(token: &str, fallback: Option<String>) -> CodexAuth {
    let claims = jwt_claims(token);
    let claim_account = claims.as_ref().and_then(|c| text(c.get("https://api.openai.com/auth")?.get("chatgpt_account_id")));
    let expires_at = claims.as_ref().and_then(|c| c.get("exp")?.as_f64()).filter(|e| e.is_finite() && *e > 0.0).map(|e| e as u64);
    CodexAuth { token: token.to_string(), account_id: claim_account.or(fallback), expires_at }
}

/// Читает вход Codex, как codex-asr: режим не ChatGPT — диктовки нет (режим ключа API так и
/// говорится); нет токена — нет входа.
pub fn read_login(path: &Path) -> Login {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Login::None,
        Err(e) => return Login::Broken(format!("auth.json не открыть: {e}")),
    };
    let v: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        // текст ошибки serde — строка и столбец, без содержимого файла
        Err(e) => return Login::Broken(format!("auth.json не читается: {e}")),
    };
    let Value::Object(m) = v else { return Login::Broken("auth.json не объект JSON".into()) };
    let mode = text(m.get("auth_mode")).or_else(|| text(m.get("authMode"))).unwrap_or_default();
    if !mode.is_empty() && !CHATGPT_MODES.contains(&mode.as_str()) {
        return if mode.to_ascii_lowercase().contains("api") { Login::ApiKey } else { Login::None };
    }
    let tokens = m.get("tokens");
    let Some(token) = text(tokens.and_then(|t| t.get("access_token"))) else {
        return if text(m.get("OPENAI_API_KEY")).is_some() { Login::ApiKey } else { Login::None };
    };
    let mut auth = auth_of_token(&token, None);
    // auth.json называет аккаунт сам; claim токена — запасной (codex-asr: сначала `tokens.account_id`)
    if let Some(a) = text(tokens.and_then(|t| t.get("account_id"))) {
        auth.account_id = Some(a);
    }
    Login::Ready(auth)
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Токен истекает в пределах `RENEW_BEFORE` (или уже истёк).
pub fn expiring(auth: &CodexAuth, now_secs: u64) -> bool {
    auth.expires_at.is_some_and(|exp| exp < now_secs + RENEW_BEFORE.as_secs())
}

// ---------- запрос ----------

/// `multipart/form-data` с одной частью `file` — тело, которое строит reqwest у codex-asr.
/// Возвращает тело и Content-Type с границей; граница гарантированно не встречается в звуке.
pub fn multipart(audio: &[u8], filename: &str, content_type: &str) -> (Vec<u8>, String) {
    let boundary = loop {
        let b = format!("----verba{}", random_hex());
        if !audio.windows(b.len()).any(|w| w == b.as_bytes()) {
            break b;
        }
    };
    let name: String = filename.chars().filter(|c| !matches!(c, '"' | '\r' | '\n')).collect();
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n"
    );
    let tail = format!("\r\n--{boundary}--\r\n");
    let mut body = Vec::with_capacity(head.len() + audio.len() + tail.len());
    body.extend_from_slice(head.as_bytes());
    body.extend_from_slice(audio);
    body.extend_from_slice(tail.as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

/// 24 случайных шестнадцатеричных знака: ключи `RandomState` берутся из ОС, без лишнего крейта.
fn random_hex() -> String {
    use std::hash::{BuildHasher, Hasher};
    static N: AtomicU64 = AtomicU64::new(0);
    let s = std::collections::hash_map::RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let a = h.finish();
    let mut h = s.build_hasher();
    h.write_u64(a);
    format!("{a:016x}{:08x}", h.finish() as u32)
}

/// 0,3 с тишины, WAV 16 кГц моно 16 бит: запись проверки — ChatGPT отвечает на неё `""` (~1 с).
pub fn silence_wav(seconds: f32) -> Vec<u8> {
    let rate: u32 = 16_000;
    let n = (rate as f32 * seconds).round() as u32;
    let data = n * 2;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    b.resize(44 + data as usize, 0);
    b
}

fn base_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(reqwest::header::ACCEPT, HeaderValue::from_static("application/json"));
    h.insert(HeaderName::from_static("originator"), HeaderValue::from_static("Codex Desktop"));
    if let Ok(ua) = HeaderValue::from_str(&user_agent()) {
        h.insert(reqwest::header::USER_AGENT, ua);
    }
    h
}

/// 401 или 403 в JSON ChatGPT — ChatGPT не принял вход; 403 не-JSON — защита сайта или путь до
/// него (страница Cloudflare через SNI-прокси, QED 06.11.2026): обновление входа не поможет.
pub fn login_refused(status: u16, json: Option<&Value>) -> bool {
    status == 401 || (status == 403 && json.is_some())
}

/// Свои слова ChatGPT: `{error: {message}}`, `{detail: "…"}` или `{detail: {message}}`.
pub fn message(json: Option<&Value>) -> String {
    let Some(j) = json else { return String::new() };
    let pick = |v: Option<&Value>| -> Option<String> {
        match v? {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("message").and_then(Value::as_str).map(str::to_string),
            _ => None,
        }
    };
    let s = pick(j.get("error")).filter(|s| !s.trim().is_empty()).or_else(|| pick(j.get("detail"))).unwrap_or_default();
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect()
}

// ---------- обновление входа ----------

/// Одно обновление входа на всё приложение (refresh-токен одноразовый): кто пришёл, пока оно идёт,
/// ждёт его итога, а не начинает второе.
pub struct Renewer {
    server: Arc<AppServer>,
    inflight: Mutex<Option<Arc<Renewal>>>,
    asked: AtomicU64,
}

/// Итог одного обновления: новый токен или слова причины; ждут его все, кто пришёл, пока оно шло.
type Renewal = tokio::sync::OnceCell<Result<String, String>>;

impl Renewer {
    pub fn new(server: Arc<AppServer>) -> Arc<Self> {
        Arc::new(Self { server, inflight: Mutex::new(None), asked: AtomicU64::new(0) })
    }

    pub fn server(&self) -> &Arc<AppServer> {
        &self.server
    }

    /// Сколько раз Codex просили обновить вход (тесты).
    pub fn asked(&self) -> u64 {
        self.asked.load(Ordering::SeqCst)
    }

    /// Новый токен от Codex (он же уже в auth.json — его пишет Codex). Ошибка — слова причины.
    pub async fn renew(&self, cmd: ServerCmd) -> Result<String, String> {
        let cell = {
            let mut g = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
            g.get_or_insert_with(|| Arc::new(Renewal::new())).clone()
        };
        let r = cell
            .get_or_init(|| async {
                self.asked.fetch_add(1, Ordering::SeqCst);
                let params = serde_json::json!({ "includeToken": true, "refreshToken": true });
                let v = self.server.request(&cmd, "getAuthStatus", params, RENEW_TIMEOUT).await?;
                match text(v.get("authToken")) {
                    Some(t) => Ok(t),
                    None => Err("Codex не отдал токен: входа ChatGPT у него нет".to_string()),
                }
            })
            .await
            .clone();
        let mut g = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            *g = None;
        }
        r
    }
}

// ---------- движок ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSetup {
    /// `https://chatgpt.com`; тесты — `http://127.0.0.1:порт`
    pub base_url: String,
    /// тот же прокси, что у OpenRouter; None — напрямую
    pub proxy: Option<String>,
    pub auth_file: PathBuf,
    /// путь к codex из настроек; None — искать (`find_codex`)
    pub program: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewWhy {
    /// токен истекал — обновление до отправки
    Expiring,
    /// ChatGPT не принял вход (статус)
    Refused(u16),
}

#[derive(Debug, Clone)]
pub struct RenewLog {
    pub why: RenewWhy,
    pub ms: u64,
    /// Ok(true) — токен новый, Ok(false) — Codex отдал прежний; Err — слова причины
    pub outcome: Result<bool, String>,
}

/// Что было со входом за диктовку: для строки лога и слов ошибки.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub renew: Option<RenewLog>,
    /// запись ушла второй раз после обновления входа; статус первого отказа
    pub resent_after: Option<u16>,
}

/// Итог проверки движка: 0,3 с тишины прошли весь путь.
#[derive(Debug, Clone)]
pub struct Checked {
    /// от первой отправки до ответа
    pub ms: u64,
    pub text: String,
    pub renewed: bool,
}

pub struct Codex {
    setup: CodexSetup,
    pool: Pool,
    renewer: Arc<Renewer>,
}

impl Codex {
    pub fn new(setup: CodexSetup, renewer: Arc<Renewer>) -> Result<Self, String> {
        let pool = Pool::new(Route { base_url: setup.base_url.clone(), proxy: setup.proxy.clone() })?;
        Ok(Self { setup, pool, renewer })
    }

    pub fn setup(&self) -> &CodexSetup {
        &self.setup
    }

    /// Открывает соединение с chatgpt.com, пока человек говорит: GET адреса распознавания без входа
    /// (ChatGPT отвечает 405) — как QED `codexWarm`.
    pub async fn warm(&self, wait: Duration) -> Warm {
        self.pool.warm(&self.pool.url(TRANSCRIBE_PATH), base_headers(), wait).await
    }

    async fn login(&self) -> Login {
        let file = self.setup.auth_file.clone();
        // диск — не в цикле сети: зависший диск не задержит отправки других диктовок
        tokio::task::spawn_blocking(move || read_login(&file))
            .await
            .unwrap_or_else(|e| Login::Broken(format!("чтение auth.json прервалось: {e}")))
    }

    async fn find(&self) -> Option<PathBuf> {
        let program = self.setup.program.clone();
        tokio::task::spawn_blocking(move || find_codex(program.as_deref())).await.ok().flatten()
    }

    async fn renew(&self, had: &CodexAuth, why: RenewWhy) -> (Option<CodexAuth>, RenewLog) {
        let started = Instant::now();
        let outcome = match self.find().await {
            None => Err(NOT_FOUND.to_string()),
            Some(program) => self.renewer.renew(ServerCmd::codex(program)).await,
        };
        let ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(token) => {
                // Codex записал новый токен в auth.json: файл называет аккаунт; claim токена — если файл отстал
                let fresh = match self.login().await {
                    Login::Ready(a) if a.token == token => a,
                    _ => auth_of_token(&token, had.account_id.clone()),
                };
                let changed = fresh.token != had.token;
                (Some(fresh), RenewLog { why, ms, outcome: Ok(changed) })
            }
            Err(e) => (None, RenewLog { why, ms, outcome: Err(e) }),
        }
    }

    async fn send(&self, auth: &CodexAuth, wav: &[u8], hedge: Hedge) -> Result<Answer, Failure> {
        let mut headers = base_headers();
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", auth.token))
            .map_err(|_| Failure::Login { words: broken("в токене недопустимые знаки") })?;
        bearer.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, bearer);
        if let Some(Ok(acc)) = auth.account_id.as_deref().map(HeaderValue::from_str) {
            headers.insert(HeaderName::from_static("chatgpt-account-id"), acc);
        }
        let (body, content_type) = multipart(wav, "voice.wav", "audio/wav");
        if let Ok(ct) = HeaderValue::from_str(&content_type) {
            headers.insert(reqwest::header::CONTENT_TYPE, ct);
        }
        let post = Post { url: self.pool.url(TRANSCRIBE_PATH), headers, body: Arc::new(body) };
        self.pool.send_hedged(post, hedge).await
    }

    /// Запись в ChatGPT. Отказ сервиса (статус ≥ 400) — `Ok(Answer)`; нет входа — `Failure::Login`.
    /// Токен на грани — сначала обновление; ChatGPT не принял вход — обновление и одна повторная
    /// отправка (если обновления до отправки не было).
    pub async fn transcribe(&self, wav: &[u8], hedge: Hedge) -> (Result<Answer, Failure>, Trace) {
        let mut trace = Trace::default();
        let had = match self.login().await {
            Login::Ready(a) => a,
            Login::ApiKey => return (Err(Failure::Login { words: api_key() }), trace),
            Login::Broken(why) => return (Err(Failure::Login { words: broken(&why) }), trace),
            Login::None => {
                let words = if self.find().await.is_some() { no_login() } else { not_installed() };
                return (Err(Failure::Login { words }), trace);
            }
        };
        let mut auth = had.clone();
        let mut renewed = false;
        if expiring(&had, now_secs()) {
            let (fresh, log) = self.renew(&had, RenewWhy::Expiring).await;
            trace.renew = Some(log);
            renewed = true;
            if let Some(a) = fresh {
                auth = a;
            }
        }
        let answer = match self.send(&auth, wav, hedge).await {
            Ok(a) => a,
            Err(f) => return (Err(f), trace),
        };
        if !login_refused(answer.status, answer.json.as_ref()) || renewed {
            return (Ok(answer), trace);
        }
        let (fresh, log) = self.renew(&auth, RenewWhy::Refused(answer.status)).await;
        trace.renew = Some(log);
        match fresh {
            Some(f) if f.token != auth.token => {
                trace.resent_after = Some(answer.status);
                (self.send(&f, wav, hedge).await, trace)
            }
            _ => (Ok(answer), trace),
        }
    }

    /// Проверка движка: 0,3 с тишины через весь путь — вход, chatgpt.com, ответ 200 с `""`.
    pub async fn check(&self, hedge: Hedge) -> Result<Checked, String> {
        let (r, trace) = self.transcribe(&silence_wav(0.3), hedge).await;
        let renewed = trace.renew.as_ref().is_some_and(|r| r.outcome == Ok(true));
        match r {
            Ok(a) if a.status == 200 => Ok(Checked { ms: a.ms, text: a.text().unwrap_or_default(), renewed }),
            Ok(a) => Err(answer_words(&a, &trace)),
            Err(f) => Err(f.words_at(Service::ChatGpt, self.setup.proxy.is_some())),
        }
    }
}

// ---------- слова (CLAUDE.md §4: «что не так → что сделать»; образец — QED `codexRefusalWords`) ----------

const NOT_FOUND: &str = "Codex не найден на этом ПК";

pub fn no_login() -> String {
    "Codex не вошёл в ChatGPT → в терминале «codex login» (вход через ChatGPT) или выбери в трее движок OpenRouter".into()
}

pub fn api_key() -> String {
    "Codex вошёл по ключу API, а распознаванию нужен вход ChatGPT → в терминале «codex logout», затем «codex login» через ChatGPT".into()
}

pub fn not_installed() -> String {
    "Codex не установлен → поставь Codex CLI («npm i -g @openai/codex») и войди через ChatGPT («codex login») или выбери в трее движок OpenRouter".into()
}

pub fn broken(why: &str) -> String {
    format!("вход Codex не прочитать ({why}) → войди в Codex заново: в терминале «codex login»")
}

/// ChatGPT не принял вход, а обновить его не вышло.
pub fn renew_failed(why: &str) -> String {
    if why == NOT_FOUND {
        format!("ChatGPT не принял вход Codex, а обновить его нечем: {NOT_FOUND} → поставь Codex CLI или укажи путь к codex в config.json (codex.program)")
    } else {
        format!("ChatGPT не принял вход Codex, а обновить его не вышло: {why} → запусти Codex, чтобы он обновил вход, или войди заново: «codex login»")
    }
}

/// Отказ ChatGPT словами; `json` — есть ли JSON в ответе: 403 без него — путь до chatgpt.com, а не вход.
pub fn refusal(status: u16, said: &str, json: bool) -> String {
    let tail = if said.is_empty() { String::new() } else { format!(": {said}") };
    match status {
        403 if !json => "chatgpt.com не пустил запрос: ответ 403 не от ChatGPT, а от защиты сайта → проверь VPN или прокси до chatgpt.com".into(),
        401 | 403 => "ChatGPT не принял вход Codex → войди в Codex заново: в терминале «codex login»".into(),
        400 if said.to_ascii_lowercase().contains("duration") => "ChatGPT не разобрал звук записи → запиши ещё раз".into(),
        408 | 504 => "ChatGPT не успел распознать запись → «вставить последнее» отправит её снова".into(),
        413 => "запись слишком большая для ChatGPT → запиши короче".into(),
        429 => "ChatGPT просит подождать: лимит подписки или частые запросы → «вставить последнее» через минуту".into(),
        s if s >= 500 => format!("ChatGPT не справился ({s}){tail} → «вставить последнее» отправит запись снова"),
        s => format!("ChatGPT отказал ({s}){tail}"),
    }
}

/// Слова ответа не-200 с учётом того, что было со входом.
pub fn answer_words(a: &Answer, trace: &Trace) -> String {
    if login_refused(a.status, a.json.as_ref()) {
        match trace.renew.as_ref().map(|r| &r.outcome) {
            Some(Err(why)) => return renew_failed(why),
            Some(Ok(_)) => {
                return "ChatGPT не принял вход Codex и после обновления → войди в Codex заново: в терминале «codex login»".into()
            }
            None => {}
        }
    }
    refusal(a.status, &message(a.json.as_ref()), a.json.is_some())
}

/// Что было со входом — для строки лога; пусто — ничего.
pub fn trace_words(t: &Trace) -> String {
    let Some(r) = &t.renew else { return String::new() };
    let sec = format!("{:.1} с", r.ms as f64 / 1000.0).replace('.', ",");
    let why = match r.why {
        RenewWhy::Expiring => "токен истекал".to_string(),
        RenewWhy::Refused(s) => format!("ChatGPT ответил {s}"),
    };
    let what = match &r.outcome {
        Ok(true) => format!("вход Codex обновлён за {sec}"),
        Ok(false) => format!("Codex вернул прежний токен за {sec}"),
        Err(e) => format!("вход Codex не обновлён за {sec}: {e}"),
    };
    let again = if t.resent_after.is_some() { ", запись отправлена ещё раз" } else { "" };
    format!("{what} ({why}){again}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}.sig", e.encode(br#"{"alg":"none"}"#), e.encode(claims.to_string()))
    }

    fn write(name: &str, v: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("verba-codex-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth.json");
        std::fs::write(&p, v).unwrap();
        p
    }

    #[test]
    fn auth_file_prefers_codex_home() {
        assert_eq!(auth_file_from(Some("D:\\cx".into()), Some("C:\\Users\\u".into())), PathBuf::from("D:\\cx\\auth.json"));
        assert_eq!(auth_file_from(Some("  ".into()), Some("C:\\Users\\u".into())), PathBuf::from("C:\\Users\\u\\.codex\\auth.json"));
        assert_eq!(auth_file_from(None, Some("C:\\Users\\u".into())), PathBuf::from("C:\\Users\\u\\.codex\\auth.json"));
    }

    #[test]
    fn chatgpt_login_with_account_from_file() {
        let t = jwt(json!({"exp": 2_000_000_000u64, "https://api.openai.com/auth": {"chatgpt_account_id": "claim-acc"}}));
        let p = write("ready", &json!({"auth_mode": "chatgpt", "tokens": {"access_token": t, "account_id": "file-acc"}}).to_string());
        let Login::Ready(a) = read_login(&p) else { panic!() };
        assert_eq!(a.token, t);
        assert_eq!(a.account_id.as_deref(), Some("file-acc"));
        assert_eq!(a.expires_at, Some(2_000_000_000));
    }

    #[test]
    fn account_falls_back_to_claim_and_modes_are_known() {
        let t = jwt(json!({"exp": 1, "https://api.openai.com/auth": {"chatgpt_account_id": "claim-acc"}}));
        for mode in ["chatgpt", "chatgpt_auth_tokens", "chatgptAuthTokens"] {
            let p = write(mode, &json!({"authMode": mode, "tokens": {"access_token": t}}).to_string());
            let Login::Ready(a) = read_login(&p) else { panic!("{mode}") };
            assert_eq!(a.account_id.as_deref(), Some("claim-acc"), "{mode}");
        }
        // режима нет, токен есть — тоже вход ChatGPT (старые auth.json)
        let p = write("nomode", &json!({"tokens": {"access_token": t}}).to_string());
        assert!(matches!(read_login(&p), Login::Ready(_)));
    }

    #[test]
    fn api_key_and_missing_logins() {
        let p = write("apikey", &json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-x"}).to_string());
        assert_eq!(read_login(&p), Login::ApiKey);
        let p = write("keyonly", &json!({"OPENAI_API_KEY": "sk-x", "tokens": null}).to_string());
        assert_eq!(read_login(&p), Login::ApiKey);
        let p = write("empty", &json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {"access_token": ""}}).to_string());
        assert_eq!(read_login(&p), Login::None);
        let p = write("bedrock", &json!({"auth_mode": "amazonBedrock"}).to_string());
        assert_eq!(read_login(&p), Login::None);
        assert_eq!(read_login(&std::env::temp_dir().join("verba-нет-такого").join("auth.json")), Login::None);
    }

    #[test]
    fn broken_file_does_not_leak_content() {
        let p = write("broken", r#"{"tokens": {"access_token": "eyJSECRET"#);
        let Login::Broken(why) = read_login(&p) else { panic!() };
        assert!(!why.contains("SECRET"), "{why}");
        let p = write("array", "[1]");
        assert!(matches!(read_login(&p), Login::Broken(_)));
    }

    #[test]
    fn debug_hides_the_token() {
        let a = auth_of_token("eyJ.SECRET.sig", None);
        assert!(!format!("{a:?}").contains("SECRET"));
        assert_eq!(a.expires_at, None, "не JWT — срока нет");
    }

    #[test]
    fn jwt_with_padding_reads() {
        let e = base64::engine::general_purpose::URL_SAFE;
        let t = format!("h.{}.s", e.encode(json!({"exp": 5}).to_string()));
        assert_eq!(auth_of_token(&t, Some("fb".into())).expires_at, Some(5));
        assert_eq!(auth_of_token(&t, Some("fb".into())).account_id.as_deref(), Some("fb"));
    }

    #[test]
    fn expiry_window_is_sixty_seconds() {
        let a = |exp| CodexAuth { token: "t".into(), account_id: None, expires_at: Some(exp) };
        assert!(expiring(&a(1030), 1000));
        assert!(expiring(&a(900), 1000));
        assert!(!expiring(&a(1061), 1000));
        assert!(!expiring(&CodexAuth { token: "t".into(), account_id: None, expires_at: None }, 1000));
    }

    #[test]
    fn multipart_is_one_file_part() {
        let audio = b"RIFF\x00\x01\r\n--x";
        let (body, ct) = multipart(audio, "voice.wav", "audio/wav");
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"voice.wav\"\r\nContent-Type: audio/wav\r\n\r\n")));
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")));
        assert!(body.windows(audio.len()).any(|w| w == audio));
        let (_, other) = multipart(audio, "voice.wav", "audio/wav");
        assert_ne!(ct, other, "граница каждый раз новая");
    }

    #[test]
    fn silence_is_a_valid_wav() {
        let w = silence_wav(0.3);
        assert_eq!(w.len(), 44 + 4800 * 2);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 9600);
    }

    #[test]
    fn refusal_words_follow_qed() {
        assert!(refusal(403, "", false).contains("VPN или прокси"));
        assert!(refusal(403, "", true).contains("codex login"));
        assert!(refusal(401, "", true).contains("codex login"));
        assert!(refusal(400, "Unable to determine audio duration", true).contains("звук"));
        for s in [400u16, 408, 413, 429, 500, 503] {
            let w = refusal(s, "", true);
            assert!(w.contains("ChatGPT"), "{s}: {w}");
        }
        assert!(refusal(413, "", true).contains("короче"));
        assert!(refusal(502, "upstream", true).contains("upstream"));
        assert_eq!(message(Some(&json!({"detail": "Unable  to\ndetermine"}))), "Unable to determine");
        assert_eq!(message(Some(&json!({"error": {"message": "bad token"}}))), "bad token");
        assert_eq!(message(Some(&json!({"detail": {"message": "m"}}))), "m");
        assert!(login_refused(401, None));
        assert!(login_refused(403, Some(&json!({}))));
        assert!(!login_refused(403, None));
        for w in [no_login(), api_key(), not_installed(), broken("x"), renew_failed("x"), renew_failed(NOT_FOUND)] {
            assert!(w.contains('→'), "{w}");
        }
    }
}
