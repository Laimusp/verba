//! Сеть распознавания: запись с повторами, прогрев соединения, проверка ключа и прокси.
//! Механизм (`Pool`) общий для обоих движков — OpenRouter (здесь) и ChatGPT по входу Codex
//! (`codex.rs`).
//!
//! Почему так (research.md §«Почему Handy подвисает»): Handy открывал новое TCP+TLS на каждую
//! диктовку и ждал единственную отправку 30 с. Путь до openrouter.ai у владельца идёт через
//! SNI-прокси, и зависшее соединение там — норма. Здесь (CLAUDE.md §3, образец — QED `codex.ts`):
//! - один долгоживущий клиент с keep-alive; при нажатии хоткея он прогревает соединение;
//! - нет ответа за 4 с + 3 с на МБ тела → та же запись уходит по новому соединению (новый клиент —
//!   иначе HTTP/2 положит повтор в то же зависшее соединение), не больше 3 отправок;
//! - побеждает первый ответ, остальные отправки отменяются; ответивший клиент становится основным;
//! - обрыв соединения или 5xx → следующая отправка сразу; 4xx не повторяется: сервис ответил.

pub mod app_server;
pub mod codex;
pub mod proxy;
pub mod service;
pub mod words;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::config::ProxyKind;

pub const MB: usize = 1024 * 1024;
/// Ответ сервиса больше — не ответ на распознавание.
const MAX_ANSWER_BYTES: usize = MB;

/// Правило повторов: ждать ответа `base_ms + per_mb_ms × МБ тела`, потом новая отправка.
#[derive(Debug, Clone, Copy)]
pub struct Hedge {
    pub base_ms: u64,
    pub per_mb_ms: u64,
    pub tries: usize,
    /// вся диктовка не дольше max(этого, 2 × tries × ожидание): каждой отправке — хотя бы два ожидания
    pub min_overall_ms: u64,
}

impl Default for Hedge {
    fn default() -> Self {
        Self { base_ms: 4000, per_mb_ms: 3000, tries: 3, min_overall_ms: 30_000 }
    }
}

impl Hedge {
    pub fn wait_ms(&self, body_bytes: usize) -> u64 {
        self.base_ms + (self.per_mb_ms as u128 * body_bytes as u128 / MB as u128) as u64
    }
    pub fn overall_ms(&self, body_bytes: usize) -> u64 {
        (2 * self.tries as u64 * self.wait_ms(body_bytes)).max(self.min_overall_ms)
    }
}

/// Куда и как ходить.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// `https://openrouter.ai`; тесты — `http://127.0.0.1:порт`
    pub base_url: String,
    /// адрес прокси с логином (`proxy::proxy_url`); None — напрямую
    pub proxy: Option<String>,
}

/// Новые соединения клиента: сколько и сколько заняло последнее (TCP + прокси + TLS).
#[derive(Default, Debug)]
pub struct ConnStats {
    pub opened: AtomicU32,
    pub last_ms: AtomicU64,
}

#[derive(Clone)]
struct TimedLayer {
    stats: Arc<ConnStats>,
}

impl<S> tower_layer::Layer<S> for TimedLayer {
    type Service = Timed<S>;
    fn layer(&self, inner: S) -> Timed<S> {
        Timed { inner, stats: self.stats.clone() }
    }
}

/// Обёртка соединителя reqwest: замеряет установку соединения для лога (CLAUDE.md §6).
#[derive(Clone)]
struct Timed<S> {
    inner: S,
    stats: Arc<ConnStats>,
}

type BoxFut<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S, R> tower_service::Service<R> for Timed<S>
where
    S: tower_service::Service<R>,
    S::Future: Send + 'static,
    S::Response: 'static,
    S::Error: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFut<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let stats = self.stats.clone();
        let started = Instant::now();
        let fut = self.inner.call(req);
        Box::pin(async move {
            let r = fut.await;
            if r.is_ok() {
                stats.last_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                stats.opened.fetch_add(1, Ordering::Relaxed);
            }
            r
        })
    }
}

/// Клиент со своим пулом соединений и счётчиком новых соединений.
pub struct Slot {
    client: reqwest::Client,
    stats: Arc<ConnStats>,
}

impl Slot {
    pub fn build(route: &Route) -> Result<Arc<Slot>, String> {
        let stats = Arc::new(ConnStats::default());
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // keep-alive: соединение ждёт следующей диктовки; сервер может закрыть раньше —
            // тогда прогрев при нажатии откроет новое, пока человек говорит
            .pool_idle_timeout(Duration::from_secs(55))
            .pool_max_idle_per_host(2)
            .tcp_keepalive(Duration::from_secs(20))
            .tcp_nodelay(true)
            .user_agent(concat!("Verba/", env!("CARGO_PKG_VERSION")))
            .connector_layer(TimedLayer { stats: stats.clone() });
        if let Some(p) = &route.proxy {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("адрес прокси не принят: {e} → проверь прокси"))?);
        }
        let client = b.build().map_err(|e| format!("сеть не настроилась: {e}"))?;
        Ok(Arc::new(Slot { client, stats }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// ответ пришёл: статус, заголовки через `headers_ms` от начала этой отправки, тело — `body_ms`
    Answered { status: u16, headers_ms: u64, body_ms: u64 },
    /// отправка оборвалась
    Failed { why: String, ms: u64 },
    /// ещё шла, когда победила другая — отменена
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptLog {
    /// через сколько мс от первой отправки ушла эта
    pub at_ms: u64,
    /// новый клиент (новое соединение), а не основной
    pub fresh: bool,
    /// за время отправки открылось новое соединение, мс (TCP + прокси + TLS); None — старое
    pub connect_ms: Option<u64>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub json: Option<serde_json::Value>,
    /// от первой отправки до заголовков ответа победителя
    pub ms: u64,
    /// номер победившей отправки (0 — первая)
    pub winner: usize,
    pub attempts: Vec<AttemptLog>,
}

impl Answer {
    pub fn text(&self) -> Option<String> {
        self.json.as_ref()?.get("text")?.as_str().map(|s| s.trim().to_string())
    }
}

#[derive(Debug, Clone)]
pub enum Failure {
    /// все отправки оборвались; `why` — причина последней
    Unreachable { why: String, attempts: Vec<AttemptLog> },
    /// никто не ответил за отведённое время
    Timeout { secs: u64, attempts: Vec<AttemptLog> },
    /// запись не ушла: нет входа или его не обновить (Codex); `words` — «что не так → что сделать»
    Login { words: String },
}

impl Failure {
    pub fn attempts(&self) -> &[AttemptLog] {
        match self {
            Failure::Unreachable { attempts, .. } | Failure::Timeout { attempts, .. } => attempts,
            Failure::Login { .. } => &[],
        }
    }
    /// Слова для OpenRouter.
    pub fn words(&self, via_proxy: bool) -> String {
        self.words_at(words::Service::OpenRouter, via_proxy)
    }
    pub fn words_at(&self, service: words::Service, via_proxy: bool) -> String {
        match self {
            Failure::Unreachable { why, .. } => words::unreachable_at(service, why, via_proxy),
            Failure::Timeout { secs, .. } => words::unreachable_at(service, &format!("нет ответа за {secs} с"), via_proxy),
            Failure::Login { words } => words.clone(),
        }
    }
}

/// Итог прогрева: соединение открыто (или было), путь жив.
#[derive(Debug, Clone)]
pub struct Warm {
    pub status: Option<u16>,
    pub ms: u64,
    /// открылось новое соединение, мс; None — использовано живое
    pub connect_ms: Option<u64>,
    /// первый прогрев молчал — клиент заменён и прогрет заново
    pub replaced: bool,
    pub error: Option<String>,
}

/// Итог одной отправки: номер, клиент, счётчик соединений клиента до неё, результат.
type Sent = (usize, Arc<Slot>, u32, Result<SendResult, String>);

struct SendResult {
    status: u16,
    json: Option<serde_json::Value>,
    headers_ms: u64,
    body_ms: u64,
}

async fn read_limited(mut resp: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| words::reqwest_why(&e))? {
        out.extend_from_slice(&chunk);
        if out.len() > MAX_ANSWER_BYTES {
            return Err("ответ слишком длинный".into());
        }
    }
    Ok(out)
}

/// Запрос распознавания, одинаковый для всех отправок одной записи.
#[derive(Clone)]
pub struct Post {
    pub url: String,
    pub headers: reqwest::header::HeaderMap,
    /// тело делится между отправками без копирования; reqwest получает по копии на отправку —
    /// для записи в сотни КБ это микросекунды
    pub body: Arc<Vec<u8>>,
}

async fn send_once(slot: Arc<Slot>, post: Post) -> Result<SendResult, String> {
    let started = Instant::now();
    let resp = slot
        .client
        .post(&post.url)
        .headers(post.headers)
        .body(reqwest::Body::from(post.body.as_ref().clone()))
        .send()
        .await
        .map_err(|e| words::reqwest_why(&e))?;
    let headers_ms = started.elapsed().as_millis() as u64;
    let status = resp.status().as_u16();
    let raw = read_limited(resp).await?;
    let body_ms = started.elapsed().as_millis() as u64 - headers_ms;
    let json = serde_json::from_slice(&raw).ok();
    Ok(SendResult { status, json, headers_ms, body_ms })
}

/// Основной клиент с keep-alive и механизм повторов поверх него — общий для обоих движков.
pub struct Pool {
    route: Route,
    primary: Mutex<Arc<Slot>>,
}

impl Pool {
    pub fn new(route: Route) -> Result<Self, String> {
        let slot = Slot::build(&route)?;
        Ok(Self { route, primary: Mutex::new(slot) })
    }

    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.route.base_url, path)
    }

    fn primary(&self) -> Arc<Slot> {
        self.primary.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_primary(&self, slot: Arc<Slot>) {
        *self.primary.lock().unwrap_or_else(|p| p.into_inner()) = slot;
    }

    fn fresh(&self) -> Result<Arc<Slot>, String> {
        Slot::build(&self.route)
    }

    /// Открывает соединение заранее маленьким GET (любой ответ — путь жив). Молчит дольше
    /// `wait` — клиент заменяется новым и прогревается ещё раз.
    pub async fn warm(&self, url: &str, headers: reqwest::header::HeaderMap, wait: Duration) -> Warm {
        let started = Instant::now();
        let mut replaced = false;
        for round in 0..2 {
            let slot = self.primary();
            let before = slot.stats.opened.load(Ordering::Relaxed);
            let req = slot.client.get(url).headers(headers.clone());
            let res = tokio::time::timeout(wait, async {
                let r = req.send().await?;
                let status = r.status().as_u16();
                let _ = r.bytes().await;
                Ok::<u16, reqwest::Error>(status)
            })
            .await;
            let connect_ms = (slot.stats.opened.load(Ordering::Relaxed) != before)
                .then(|| slot.stats.last_ms.load(Ordering::Relaxed));
            match res {
                Ok(Ok(status)) => {
                    return Warm { status: Some(status), ms: started.elapsed().as_millis() as u64, connect_ms, replaced, error: None }
                }
                Ok(Err(e)) if round == 1 => {
                    return Warm {
                        status: None,
                        ms: started.elapsed().as_millis() as u64,
                        connect_ms,
                        replaced,
                        error: Some(words::reqwest_why(&e)),
                    }
                }
                Err(_) if round == 1 => {
                    return Warm {
                        status: None,
                        ms: started.elapsed().as_millis() as u64,
                        connect_ms,
                        replaced,
                        error: Some(format!("нет ответа за {} с", wait.as_secs())),
                    }
                }
                // зависло или оборвалось: соединение основного клиента, видимо, мёртвое — заменить
                _ => {
                    if let Ok(s) = self.fresh() {
                        self.set_primary(s);
                        replaced = true;
                    }
                }
            }
        }
        unreachable!("цикл прогрева возвращает на втором круге")
    }

    /// Статус маленького GET через основной клиент.
    pub async fn get_status(&self, url: &str, headers: reqwest::header::HeaderMap, wait: Duration) -> Result<u16, String> {
        let slot = self.primary();
        let fut = async {
            let r = slot.client.get(url).headers(headers).send().await.map_err(|e| words::reqwest_why(&e))?;
            Ok(r.status().as_u16())
        };
        tokio::time::timeout(wait, fut).await.map_err(|_| format!("нет ответа за {} с", wait.as_secs()))?
    }

    /// Запись с повторами. Отказ сервиса (статус ≥ 400) — это `Ok(Answer)`.
    pub async fn send_hedged(&self, post: Post, hedge: Hedge) -> Result<Answer, Failure> {
        let wait = Duration::from_millis(hedge.wait_ms(post.body.len()));
        let overall_ms = hedge.overall_ms(post.body.len());
        let started = Instant::now();
        let mut set: tokio::task::JoinSet<Sent> = tokio::task::JoinSet::new();
        let mut attempts: Vec<AttemptLog> = Vec::new();
        let mut running = 0usize;
        let mut last_why = String::new();
        let mut last_5xx: Option<(usize, Arc<Slot>, SendResult)> = None;

        let launch = |attempts: &mut Vec<AttemptLog>, set: &mut tokio::task::JoinSet<_>| -> bool {
            let idx = attempts.len();
            let slot = if idx == 0 {
                self.primary()
            } else {
                match self.fresh() {
                    Ok(s) => s,
                    Err(_) => return false,
                }
            };
            let before = slot.stats.opened.load(Ordering::Relaxed);
            attempts.push(AttemptLog {
                at_ms: started.elapsed().as_millis() as u64,
                fresh: idx > 0,
                connect_ms: None,
                outcome: Outcome::Dropped,
            });
            let post = post.clone();
            set.spawn(async move {
                let r = send_once(slot.clone(), post).await;
                (idx, slot, before, r)
            });
            true
        };

        if launch(&mut attempts, &mut set) {
            running += 1;
        }
        let deadline = tokio::time::sleep(Duration::from_millis(overall_ms));
        tokio::pin!(deadline);
        let hedge_timer = tokio::time::sleep(wait);
        tokio::pin!(hedge_timer);

        loop {
            let can_launch = attempts.len() < hedge.tries;
            tokio::select! {
                joined = set.join_next(), if running > 0 => {
                    let Some(joined) = joined else { continue };
                    running -= 1;
                    let Ok((idx, slot, before, result)) = joined else {
                        // задача отправки упала (паника) — как обрыв
                        last_why = "отправка прервалась".into();
                        if can_launch && launch(&mut attempts, &mut set) {
                            running += 1;
                            hedge_timer.as_mut().reset(tokio::time::Instant::now() + wait);
                        }
                        continue;
                    };
                    if slot.stats.opened.load(Ordering::Relaxed) != before {
                        attempts[idx].connect_ms = Some(slot.stats.last_ms.load(Ordering::Relaxed));
                    }
                    match result {
                        Ok(r) if r.status >= 500 && (can_launch || running > 0) => {
                            attempts[idx].outcome = Outcome::Answered { status: r.status, headers_ms: r.headers_ms, body_ms: r.body_ms };
                            last_5xx = Some((idx, slot, r));
                            if can_launch && launch(&mut attempts, &mut set) {
                                running += 1;
                                hedge_timer.as_mut().reset(tokio::time::Instant::now() + wait);
                            }
                        }
                        Ok(r) => {
                            attempts[idx].outcome = Outcome::Answered { status: r.status, headers_ms: r.headers_ms, body_ms: r.body_ms };
                            set.abort_all();
                            if idx > 0 {
                                self.set_primary(slot);
                            }
                            return Ok(Answer {
                                status: r.status,
                                json: r.json,
                                ms: attempts[idx].at_ms + r.headers_ms,
                                winner: idx,
                                attempts,
                            });
                        }
                        Err(why) => {
                            attempts[idx].outcome = Outcome::Failed { why: why.clone(), ms: started.elapsed().as_millis() as u64 - attempts[idx].at_ms };
                            last_why = why;
                            // обрыв — следующая отправка сразу, не дожидаясь таймера
                            if can_launch && launch(&mut attempts, &mut set) {
                                running += 1;
                                hedge_timer.as_mut().reset(tokio::time::Instant::now() + wait);
                            }
                        }
                    }
                    if running == 0 {
                        // отправок больше нет и никто не ответил по делу
                        if let Some((idx, slot, r)) = last_5xx.take() {
                            if idx > 0 {
                                self.set_primary(slot);
                            }
                            return Ok(Answer { status: r.status, json: r.json, ms: attempts[idx].at_ms + r.headers_ms, winner: idx, attempts });
                        }
                        if let Ok(s) = self.fresh() {
                            self.set_primary(s);
                        }
                        return Err(Failure::Unreachable { why: last_why, attempts });
                    }
                }
                _ = &mut hedge_timer, if can_launch => {
                    // нет ответа вовремя — та же запись по новому соединению; прежние отправки идут дальше
                    if launch(&mut attempts, &mut set) {
                        running += 1;
                    }
                    hedge_timer.as_mut().reset(tokio::time::Instant::now() + wait);
                }
                _ = &mut deadline => {
                    set.abort_all();
                    if let Ok(s) = self.fresh() {
                        self.set_primary(s);
                    }
                    return Err(Failure::Timeout { secs: overall_ms / 1000, attempts });
                }
            }
        }
    }
}

fn bearer(key: &str) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Ok(mut v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
        v.set_sensitive(true);
        h.insert(reqwest::header::AUTHORIZATION, v);
    }
    h
}

pub struct OpenRouter {
    pool: Pool,
}

impl OpenRouter {
    pub fn new(route: Route) -> Result<Self, String> {
        Ok(Self { pool: Pool::new(route)? })
    }

    pub fn route(&self) -> &Route {
        self.pool.route()
    }

    /// JSON-тело распознавания, как в QED (`service.ts:403`).
    pub fn transcription_body(model: &str, wav: &[u8]) -> Vec<u8> {
        let data = base64::engine::general_purpose::STANDARD.encode(wav);
        serde_json::to_vec(&serde_json::json!({
            "model": model,
            "input_audio": { "data": data, "format": "wav" },
        }))
        .unwrap_or_default()
    }

    /// Открывает соединение заранее: GET /api/v1/key (маленький ответ; заодно проверка ключа).
    /// Молчит дольше `wait` — клиент заменяется новым и прогревается ещё раз.
    pub async fn warm(&self, key: &str, wait: Duration) -> Warm {
        let headers = if key.is_empty() { reqwest::header::HeaderMap::new() } else { bearer(key) };
        self.pool.warm(&self.pool.url("/api/v1/key"), headers, wait).await
    }

    /// Проверка ключа: статус GET /api/v1/key (200 — принят, 401/403 — нет).
    pub async fn check_key(&self, key: &str, wait: Duration) -> Result<u16, String> {
        self.pool.get_status(&self.pool.url("/api/v1/key"), bearer(key), wait).await
    }

    /// Распознавание записи с повторами. Отказ сервиса (статус ≥ 400) — это `Ok(Answer)`.
    pub async fn transcribe(&self, key: &str, model: &str, wav: &[u8], hedge: Hedge) -> Result<Answer, Failure> {
        let mut headers = bearer(key);
        headers.insert(reqwest::header::CONTENT_TYPE, reqwest::header::HeaderValue::from_static("application/json"));
        headers.insert(reqwest::header::ACCEPT, reqwest::header::HeaderValue::from_static("application/json"));
        let post = Post {
            url: self.pool.url("/api/v1/audio/transcriptions"),
            headers,
            body: Arc::new(Self::transcription_body(model, wav)),
        };
        self.pool.send_hedged(post, hedge).await
    }
}

// Вид прокси, когда строка его не назвала: HTTP, SOCKS5 и SOCKS4 пробуются сразу, побеждает
/// первый, через который OpenRouter ответил хоть чем-то (как QED `service.ts:255`).
pub async fn detect_proxy_kind(
    base_url: &str,
    host: &str,
    port: u16,
    user: Option<&str>,
    pass: Option<&str>,
    wait: Duration,
) -> Option<ProxyKind> {
    let mut set = tokio::task::JoinSet::new();
    for kind in [ProxyKind::Http, ProxyKind::Socks5, ProxyKind::Socks4] {
        let route = Route { base_url: base_url.to_string(), proxy: Some(proxy::proxy_url(kind, host, port, user, pass)) };
        set.spawn(async move {
            let slot = Slot::build(&route).ok()?;
            let r = slot.client.get(format!("{}/api/v1/key", route.base_url)).send().await.ok()?;
            // HTTP-прокси, которому дали SOCKS-рукопожатие, может ответить своей страницей —
            // засчитываем только ответ, похожий на OpenRouter (JSON)
            let json = r.headers().get(reqwest::header::CONTENT_TYPE).is_some_and(|v| v.to_str().unwrap_or("").contains("json"));
            json.then_some(kind)
        });
    }
    let found = tokio::time::timeout(wait, async {
        while let Some(r) = set.join_next().await {
            if let Ok(Some(kind)) = r {
                return Some(kind);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    set.abort_all();
    found
}

/// Строка лога о том, куда ушло время сети (без ключей и текста).
pub fn timing_words(attempts: &[AttemptLog]) -> String {
    let sec = |ms: u64| format!("{:.2} с", ms as f64 / 1000.0).replace('.', ",");
    attempts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let conn = match a.connect_ms {
                Some(ms) => format!("новое соединение {}", sec(ms)),
                None => "соединение прежнее".into(),
            };
            let what = match &a.outcome {
                Outcome::Answered { status, headers_ms, body_ms } => {
                    format!("ответ {status} за {} (+тело {})", sec(*headers_ms), sec(*body_ms))
                }
                Outcome::Failed { why, ms } => format!("оборвалась через {}: {why}", sec(*ms)),
                Outcome::Dropped => "отменена: ответила другая".into(),
            };
            format!("#{} в {}{}: {conn}, {what}", i + 1, sec(a.at_ms), if a.fresh { " (новый клиент)" } else { "" })
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hedge_wait_grows_with_body() {
        let h = Hedge::default();
        assert_eq!(h.wait_ms(0), 4000);
        assert_eq!(h.wait_ms(MB), 7000);
        assert_eq!(h.wait_ms(MB / 2), 5500);
        assert_eq!(h.overall_ms(0), 30_000);
        assert_eq!(h.overall_ms(4 * MB), 2 * 3 * 16_000);
    }

    #[test]
    fn body_is_qed_shaped() {
        let b = OpenRouter::transcription_body("m/x", b"RIFF");
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["model"], "m/x");
        assert_eq!(v["input_audio"]["format"], "wav");
        assert_eq!(v["input_audio"]["data"], "UklGRg==");
    }
}
