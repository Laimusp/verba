//! Движок «ChatGPT по входу Codex» против фейков (CLAUDE.md «Проверка»: не живые сервисы):
//! фейковый chatgpt.com на 127.0.0.1 и фейковый `codex app-server` — этот же тестовый exe,
//! запущенный через `.cmd` (тот же путь через cmd.exe, что у настоящего `codex.cmd` из npm).
//! Настоящий Codex тесты не зовут никогда: путь к codex задан явно, auth.json — во временной папке.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use verba::net::app_server::{AppServer, ServerCmd, Timeouts};
use verba::net::codex::{self, Codex, CodexSetup, RenewWhy, Renewer};
use verba::net::words::Service;
use verba::net::{Failure, Hedge};

// ---------- фейковый chatgpt.com ----------

#[derive(Clone, Debug)]
enum Act {
    /// прочитать запрос и молчать, держа соединение
    Hang,
    /// ответить JSON
    Json(u16, &'static str),
    /// ответить страницей не-JSON (защита сайта)
    Html(u16),
}

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

struct Fake {
    url: String,
    connections: Arc<AtomicUsize>,
    reqs: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    fn reqs(&self) -> Vec<Req> {
        self.reqs.lock().unwrap().clone()
    }
}

async fn read_request(s: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first: Vec<String> = lines.next()?.split_whitespace().map(str::to_string).collect();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(Req {
        method: first.first().cloned().unwrap_or_default(),
        path: first.get(1).cloned().unwrap_or_default(),
        headers,
        body: buf[head_end..head_end + len].to_vec(),
    })
}

async fn fake(script: Vec<Act>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(script);
    let (c, r) = (connections.clone(), reqs.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            c.fetch_add(1, Ordering::SeqCst);
            let (script, r) = (script.clone(), r.clone());
            tokio::spawn(async move {
                while let Some(req) = read_request(&mut sock).await {
                    let i = {
                        let mut g = r.lock().unwrap();
                        g.push(req);
                        g.len() - 1
                    };
                    let (status, ctype, body) = match script.get(i).cloned().unwrap_or(Act::Json(200, r#"{"text":"лишний"}"#)) {
                        Act::Hang => {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                            return;
                        }
                        Act::Json(s, b) => (s, "application/json", b.to_string()),
                        Act::Html(s) => (s, "text/html", "<html><body>Just a moment...</body></html>".to_string()),
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
                        body.len()
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Fake { url, connections, reqs }
}

// ---------- фейковый codex app-server ----------

/// Не тест: фейковый `codex app-server`, когда этот exe запущен с VERBA_FAKE_APP_SERVER
/// (из `.cmd`, который пишет `fake_codex`). Режимы: `ok` — отдаёт VERBA_FAKE_TOKEN; `error` —
/// отказ обновления; `null` — токена нет; `hang-init` — молчит на initialize.
#[test]
fn fake_app_server() {
    let Ok(mode) = std::env::var("VERBA_FAKE_APP_SERVER") else { return };
    let token = std::env::var("VERBA_FAKE_TOKEN").unwrap_or_default();
    let delay = std::env::var("VERBA_FAKE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0u64);
    let log = std::env::var("VERBA_FAKE_LOG").unwrap_or_default();
    let note = |what: &str| {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            let _ = writeln!(f, "{what}");
        }
    };
    note("start");
    let out = std::io::stdout();
    let send = |v: Value| {
        let mut o = out.lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    };
    // строка лога на stdout — клиент её пропускает
    send(json!("not an object"));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
        if m["id"] == "srv-1" && m.get("error").is_some() {
            note("server-request-answered");
            continue;
        }
        let id = m["id"].clone();
        match m["method"].as_str().unwrap_or("") {
            "initialize" => {
                if mode == "hang-init" {
                    std::thread::sleep(Duration::from_secs(3600));
                }
                // уведомление и запрос сервера к клиенту: клиент не ждёт и отвечает ошибкой
                send(json!({"method": "account/updated", "params": {}}));
                send(json!({"id": "srv-1", "method": "item/commandExecution/requestApproval", "params": {}}));
                send(json!({"id": id, "result": {"userAgent": "fake"}}));
            }
            "initialized" => note("initialized"),
            "getAuthStatus" => {
                note(&format!("getAuthStatus {}", m["params"]));
                std::thread::sleep(Duration::from_millis(delay));
                match mode.as_str() {
                    "error" => send(json!({"id": id, "error": {"code": -32000, "message": "refresh token expired"}})),
                    "null" => send(json!({"id": id, "result": {"authToken": null, "authMethod": null}})),
                    _ => send(json!({"id": id, "result": {"authToken": token, "authMethod": "chatgpt"}})),
                }
            }
            _ => send(json!({"id": id, "error": {"code": -32601, "message": "unknown"}})),
        }
    }
    note("exit");
}

struct FakeCodex {
    cmd: PathBuf,
    log: PathBuf,
}

impl FakeCodex {
    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(str::to_string).collect()
    }
    fn count(&self, what: &str) -> usize {
        self.log().iter().filter(|l| l.starts_with(what)).count()
    }
}

/// `.cmd`, запускающий фейковый app-server: режим и токен — в самом файле.
fn fake_codex(dir: &Path, mode: &str, token: &str, delay_ms: u64) -> FakeCodex {
    let exe = std::env::current_exe().unwrap();
    let log = dir.join("app-server.log");
    let cmd = dir.join("codex.cmd");
    let text = format!(
        "@echo off\r\nset \"VERBA_FAKE_APP_SERVER={mode}\"\r\nset \"VERBA_FAKE_TOKEN={token}\"\r\nset \"VERBA_FAKE_DELAY_MS={delay_ms}\"\r\nset \"VERBA_FAKE_LOG={}\"\r\n\"{}\" fake_app_server --exact --nocapture --test-threads=1 -q\r\n",
        log.display(),
        exe.display()
    );
    std::fs::write(&cmd, text).unwrap();
    FakeCodex { cmd, log }
}

// ---------- окружение теста ----------

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn jwt(exp: u64, tag: &str) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({"exp": exp, "tag": tag, "https://api.openai.com/auth": {"chatgpt_account_id": "acc-claim"}});
    format!("{}.{}.sig", e.encode(br#"{"alg":"none"}"#), e.encode(claims.to_string()))
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("verba-test-codex-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_auth(dir: &Path, token: &str) -> PathBuf {
    let p = dir.join("auth.json");
    let v = json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {"access_token": token, "account_id": "acc-file", "refresh_token": "r"}});
    std::fs::write(&p, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    p
}

fn timeouts() -> Timeouts {
    Timeouts { idle: Duration::from_secs(5), start: Duration::from_secs(20), exit_grace: Duration::from_secs(2) }
}

fn engine(f: &Fake, auth_file: PathBuf, program: PathBuf, renewer: Arc<Renewer>) -> Codex {
    Codex::new(CodexSetup { base_url: f.url.clone(), proxy: None, auth_file, program: Some(program) }, renewer).unwrap()
}

fn hedge(base_ms: u64) -> Hedge {
    Hedge { base_ms, per_mb_ms: 0, tries: 3, min_overall_ms: 10_000 }
}

const WAV: &[u8] = b"RIFF\x24\x00\x00\x00WAVEfmt fake-audio-bytes";

// ---------- тесты ----------

#[tokio::test]
async fn sends_multipart_with_codex_headers() {
    let dir = temp_dir("headers");
    let token = jwt(now() + 86_400, "a");
    let auth = write_auth(&dir, &token);
    let f = fake(vec![Act::Json(200, r#"{"text":" привет мир ","asset_ttl":"30d"}"#)]).await;
    let c = engine(&f, auth, dir.join("нет-codex.cmd"), Renewer::new(AppServer::new(timeouts())));
    let (r, trace) = c.transcribe(WAV, hedge(2000)).await;
    let a = r.unwrap();
    assert_eq!(a.status, 200);
    assert_eq!(a.text().as_deref(), Some("привет мир"));
    assert!(trace.renew.is_none());
    let req = &f.reqs()[0];
    assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/backend-api/transcribe"));
    assert_eq!(req.header("authorization"), Some(format!("Bearer {token}").as_str()));
    assert_eq!(req.header("chatgpt-account-id"), Some("acc-file"), "аккаунт из auth.json, а не из токена");
    assert_eq!(req.header("originator"), Some("Codex Desktop"));
    assert!(req.header("user-agent").unwrap().starts_with("Codex Desktop/26.429.30905 (windows;"));
    let ct = req.header("content-type").unwrap();
    let boundary = ct.strip_prefix("multipart/form-data; boundary=").expect(ct);
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains("name=\"file\"; filename=\"voice.wav\"\r\nContent-Type: audio/wav\r\n\r\n"), "{body}");
    assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    assert!(req.body.windows(WAV.len()).any(|w| w == WAV));
}

#[tokio::test]
async fn refusals_are_words_and_not_renewed() {
    let dir = temp_dir("refusals");
    let auth = write_auth(&dir, &jwt(now() + 86_400, "a"));
    let fc = fake_codex(&dir, "ok", &jwt(now() + 86_400, "b"), 0);
    let cases: Vec<(Act, &str)> = vec![
        (Act::Json(400, r#"{"detail":"Unable to determine audio duration"}"#), "звук"),
        (Act::Json(413, r#"{"error":{"message":"too large"}}"#), "короче"),
        (Act::Json(429, r#"{"detail":"Too many requests"}"#), "подождать"),
        (Act::Html(403), "VPN или прокси"),
    ];
    for (act, want) in cases {
        let f = fake(vec![act.clone()]).await;
        let renewer = Renewer::new(AppServer::new(timeouts()));
        let c = engine(&f, auth.clone(), fc.cmd.clone(), renewer.clone());
        let (r, trace) = c.transcribe(WAV, hedge(2000)).await;
        let a = r.unwrap();
        let words = codex::answer_words(&a, &trace);
        assert!(words.contains(want), "{act:?}: {words}");
        assert!(words.contains('→'), "{words}");
        assert_eq!(renewer.asked(), 0, "{act:?}: вход не виноват — не обновлять");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(f.reqs().len(), 1, "{act:?}: отказ сервиса не повторяется");
    }
    assert_eq!(fc.count("start"), 0, "app-server не запускался");
}

#[tokio::test]
async fn server_errors_go_again_then_become_words() {
    let dir = temp_dir("5xx");
    let auth = write_auth(&dir, &jwt(now() + 86_400, "a"));
    let e = Act::Json(502, r#"{"detail":"upstream"}"#);
    let f = fake(vec![e.clone(), e.clone(), e]).await;
    let c = engine(&f, auth, dir.join("нет.cmd"), Renewer::new(AppServer::new(timeouts())));
    let (r, trace) = c.transcribe(WAV, hedge(5000)).await;
    let a = r.unwrap();
    assert_eq!(a.status, 502);
    assert_eq!(a.attempts.len(), 3);
    let words = codex::answer_words(&a, &trace);
    assert!(words.contains("upstream") && words.contains("вставить последнее"), "{words}");
}

#[tokio::test]
async fn refused_login_is_renewed_and_sent_once_more() {
    for first in [Act::Json(401, r#"{"error":{"message":"Invalid token"}}"#), Act::Json(403, r#"{"detail":"forbidden"}"#)] {
        let dir = temp_dir(&format!("renew-{}", matches!(first, Act::Json(401, _))));
        let old = jwt(now() + 86_400, "old");
        let new = jwt(now() + 10 * 86_400, "new");
        let auth = write_auth(&dir, &old);
        let before = (std::fs::read(&auth).unwrap(), std::fs::metadata(&auth).unwrap().modified().unwrap());
        let fc = fake_codex(&dir, "ok", &new, 0);
        let f = fake(vec![first.clone(), Act::Json(200, r#"{"text":"после обновления"}"#)]).await;
        let renewer = Renewer::new(AppServer::new(timeouts()));
        let c = engine(&f, auth.clone(), fc.cmd.clone(), renewer.clone());
        let (r, trace) = c.transcribe(WAV, hedge(2000)).await;
        let a = r.unwrap();
        assert_eq!(a.text().as_deref(), Some("после обновления"), "{first:?}");
        let reqs = f.reqs();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].header("authorization"), Some(format!("Bearer {old}").as_str()));
        assert_eq!(reqs[1].header("authorization"), Some(format!("Bearer {new}").as_str()));
        // Codex не переписал файл (фейк его не пишет) — аккаунт берётся из прежнего входа
        assert_eq!(reqs[1].header("chatgpt-account-id"), Some("acc-claim"));
        let log = trace.renew.unwrap();
        assert!(matches!(log.why, RenewWhy::Refused(401 | 403)));
        assert_eq!(log.outcome, Ok(true));
        assert!(trace.resent_after.is_some());
        assert_eq!(renewer.asked(), 1);
        assert_eq!(fc.count("getAuthStatus {\"includeToken\":true,\"refreshToken\":true}"), 1, "{:?}", fc.log());
        assert_eq!(fc.count("server-request-answered"), 1, "запрос сервера к клиенту получил ответ-ошибку");
        let after = (std::fs::read(&auth).unwrap(), std::fs::metadata(&auth).unwrap().modified().unwrap());
        assert_eq!(before, after, "Verba auth.json не пишет");
        renewer.server().close().await;
    }
}

#[tokio::test]
async fn expiring_token_is_renewed_before_the_send() {
    let dir = temp_dir("expiring");
    let old = jwt(now() + 30, "old");
    let new = jwt(now() + 10 * 86_400, "new");
    let auth = write_auth(&dir, &old);
    let fc = fake_codex(&dir, "ok", &new, 0);
    let f = fake(vec![Act::Json(200, r#"{"text":"ок"}"#)]).await;
    let renewer = Renewer::new(AppServer::new(timeouts()));
    let c = engine(&f, auth, fc.cmd.clone(), renewer.clone());
    let (r, trace) = c.transcribe(WAV, hedge(2000)).await;
    assert_eq!(r.unwrap().text().as_deref(), Some("ок"));
    let reqs = f.reqs();
    assert_eq!(reqs.len(), 1, "одна отправка — уже с новым токеном");
    assert_eq!(reqs[0].header("authorization"), Some(format!("Bearer {new}").as_str()));
    assert_eq!(trace.renew.as_ref().map(|r| r.why), Some(RenewWhy::Expiring));
    assert!(codex::trace_words(&trace).contains("токен истекал"));
    renewer.server().close().await;
}

#[tokio::test]
async fn concurrent_dictations_share_one_renewal() {
    let dir = temp_dir("single-flight");
    let auth = write_auth(&dir, &jwt(now() + 10, "old"));
    let fc = fake_codex(&dir, "ok", &jwt(now() + 86_400, "new"), 400);
    let f = fake(vec![]).await;
    let renewer = Renewer::new(AppServer::new(timeouts()));
    let c = Arc::new(engine(&f, auth, fc.cmd.clone(), renewer.clone()));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..3 {
        let c = c.clone();
        set.spawn(async move { c.transcribe(WAV, hedge(2000)).await });
    }
    while let Some(r) = set.join_next().await {
        let (r, trace) = r.unwrap();
        assert_eq!(r.unwrap().status, 200);
        assert_eq!(trace.renew.unwrap().outcome, Ok(true));
    }
    assert_eq!(fc.count("getAuthStatus"), 1, "refresh-токен одноразовый: обновление одно");
    assert_eq!(fc.count("start"), 1);
    renewer.server().close().await;
}

#[tokio::test]
async fn failed_renewal_says_why() {
    let dir = temp_dir("renew-error");
    let auth = write_auth(&dir, &jwt(now() + 86_400, "a"));
    let fc = fake_codex(&dir, "error", "", 0);
    let f = fake(vec![Act::Json(401, r#"{"error":{"message":"bad"}}"#)]).await;
    let renewer = Renewer::new(AppServer::new(timeouts()));
    let c = engine(&f, auth, fc.cmd.clone(), renewer.clone());
    let (r, trace) = c.transcribe(WAV, hedge(2000)).await;
    let a = r.unwrap();
    assert_eq!(a.status, 401);
    assert_eq!(f.reqs().len(), 1, "нового токена нет — повторять незачем");
    let words = codex::answer_words(&a, &trace);
    assert!(words.contains("refresh token expired") && words.contains("codex login"), "{words}");
    renewer.server().close().await;

    // Codex ответил, но токена не отдал
    let fc = fake_codex(&dir, "null", "", 0);
    let f = fake(vec![Act::Json(401, "{}")]).await;
    let renewer = Renewer::new(AppServer::new(timeouts()));
    let c = engine(&f, dir.join("auth.json"), fc.cmd.clone(), renewer.clone());
    let (r, trace) = c.transcribe(WAV, hedge(2000)).await;
    let words = codex::answer_words(&r.unwrap(), &trace);
    assert!(words.contains("не отдал токен"), "{words}");
    renewer.server().close().await;
}

#[tokio::test]
async fn missing_codex_and_logins_are_words_without_sending() {
    let dir = temp_dir("logins");
    let f = fake(vec![Act::Json(401, "{}")]).await;
    let missing = dir.join("нет-codex.cmd");
    let renewer = Renewer::new(AppServer::new(timeouts()));

    // нет auth.json и нет Codex — «не установлен»
    let c = engine(&f, dir.join("auth.json"), missing.clone(), renewer.clone());
    let (r, _) = c.transcribe(WAV, hedge(2000)).await;
    let Err(Failure::Login { words }) = r else { panic!("{r:?}") };
    assert!(words.contains("не установлен"), "{words}");

    // нет auth.json, Codex есть — «не вошёл»
    let fc = fake_codex(&dir, "ok", "", 0);
    let c = engine(&f, dir.join("auth.json"), fc.cmd.clone(), renewer.clone());
    let (r, _) = c.transcribe(WAV, hedge(2000)).await;
    let Err(Failure::Login { words }) = r else { panic!("{r:?}") };
    assert!(words.contains("не вошёл"), "{words}");

    // вход по ключу API
    let key = dir.join("key.json");
    std::fs::write(&key, r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#).unwrap();
    let c = engine(&f, key, fc.cmd.clone(), renewer.clone());
    let (r, _) = c.transcribe(WAV, hedge(2000)).await;
    let Err(Failure::Login { words }) = r else { panic!("{r:?}") };
    assert!(words.contains("ключу API"), "{words}");

    // битый auth.json
    let broken = dir.join("broken.json");
    std::fs::write(&broken, "{ not json").unwrap();
    let c = engine(&f, broken, fc.cmd.clone(), renewer.clone());
    let (r, _) = c.transcribe(WAV, hedge(2000)).await;
    let Err(f2) = r else { panic!() };
    assert!(f2.words_at(Service::ChatGpt, false).contains("codex login"));
    assert_eq!(f.reqs().len(), 0, "без входа запись не уходит");

    // токен истёк, а Codex нет: отправка со старым токеном, 401 → слова «обновить нечем»
    let auth = write_auth(&dir, &jwt(now().saturating_sub(5), "old"));
    let started = Instant::now();
    let c = engine(&f, auth, missing, renewer.clone());
    let (r, trace) = c.transcribe(WAV, hedge(2000)).await;
    let words = codex::answer_words(&r.unwrap(), &trace);
    assert!(words.contains("Codex не найден") && words.contains("codex.program"), "{words}");
    assert!(started.elapsed() < Duration::from_secs(3), "без Codex — без ожиданий");
    assert_eq!(renewer.server().starts(), 0);
}

#[tokio::test]
async fn app_server_closes_after_idle_and_starts_again() {
    let dir = temp_dir("idle");
    let fc = fake_codex(&dir, "ok", "tok", 0);
    let server = AppServer::new(Timeouts { idle: Duration::from_millis(400), ..timeouts() });
    let renewer = Renewer::new(server.clone());
    let cmd = ServerCmd::codex(fc.cmd.clone());
    assert_eq!(renewer.renew(cmd.clone()).await, Ok("tok".into()));
    assert!(server.running().await);
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.running().await {
        assert!(Instant::now() < deadline, "app-server не закрылся после простоя");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // закрытый stdin — фейк выходит сам
    let deadline = Instant::now() + Duration::from_secs(5);
    while fc.count("exit") == 0 {
        assert!(Instant::now() < deadline, "фейк не вышел: {:?}", fc.log());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(renewer.renew(cmd).await, Ok("tok".into()));
    assert_eq!(server.starts(), 2);
    assert_eq!(fc.count("initialized"), 2, "после initialize — уведомление initialized");
    server.close().await;
}

#[tokio::test]
async fn hung_app_server_start_times_out_and_is_killed() {
    let dir = temp_dir("hang");
    let fc = fake_codex(&dir, "hang-init", "", 0);
    let server = AppServer::new(Timeouts { start: Duration::from_millis(800), exit_grace: Duration::from_millis(300), ..timeouts() });
    let renewer = Renewer::new(server.clone());
    let started = Instant::now();
    let r = renewer.renew(ServerCmd::codex(fc.cmd.clone())).await;
    assert!(r.as_ref().unwrap_err().contains("не ответил на старт"), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(!server.running().await);
    // следующий запрос — с нового процесса, а не в зависший
    let r = renewer.renew(ServerCmd::codex(fc.cmd.clone())).await;
    assert!(r.is_err());
    assert_eq!(server.starts(), 2);
}

#[tokio::test]
async fn check_sends_silence_and_takes_empty_text() {
    let dir = temp_dir("check");
    let auth = write_auth(&dir, &jwt(now() + 86_400, "a"));
    let f = fake(vec![Act::Json(200, r#"{"text":""}"#)]).await;
    let c = engine(&f, auth, dir.join("нет.cmd"), Renewer::new(AppServer::new(timeouts())));
    let ch = c.check(hedge(2000)).await.unwrap();
    assert_eq!(ch.text, "");
    assert!(!ch.renewed);
    let req = &f.reqs()[0];
    assert!(req.body.windows(4).any(|w| w == b"RIFF"));
    assert!(req.body.len() > 9644, "0,3 с тишины 16 кГц — 9644 байта WAV");
}

#[tokio::test]
async fn warm_connection_carries_the_recording() {
    let dir = temp_dir("warm");
    let auth = write_auth(&dir, &jwt(now() + 86_400, "a"));
    let f = fake(vec![Act::Json(405, r#"{"detail":"Method Not Allowed"}"#), Act::Json(200, r#"{"text":"по прогретому"}"#)]).await;
    let c = engine(&f, auth, dir.join("нет.cmd"), Renewer::new(AppServer::new(timeouts())));
    let w = c.warm(Duration::from_secs(2)).await;
    assert_eq!(w.status, Some(405));
    assert!(w.connect_ms.is_some());
    let (r, _) = c.transcribe(WAV, hedge(2000)).await;
    let a = r.unwrap();
    assert_eq!(a.text().as_deref(), Some("по прогретому"));
    assert_eq!(a.attempts[0].connect_ms, None, "запись идёт по прогретому соединению");
    assert_eq!(f.connections.load(Ordering::SeqCst), 1);
    let reqs = f.reqs();
    assert_eq!((reqs[0].method.as_str(), reqs[0].path.as_str()), ("GET", "/backend-api/transcribe"));
    assert_eq!(reqs[0].header("authorization"), None, "прогрев без входа");
}

#[tokio::test]
async fn hung_send_goes_again_with_the_same_body() {
    let dir = temp_dir("hedge");
    let auth = write_auth(&dir, &jwt(now() + 86_400, "a"));
    let f = fake(vec![Act::Hang, Act::Json(200, r#"{"text":"второй"}"#)]).await;
    let c = engine(&f, auth, dir.join("нет.cmd"), Renewer::new(AppServer::new(timeouts())));
    let (r, _) = c.transcribe(WAV, hedge(300)).await;
    let a = r.unwrap();
    assert_eq!(a.text().as_deref(), Some("второй"));
    assert_eq!(a.winner, 1);
    let reqs = f.reqs();
    assert_eq!(reqs[0].body, reqs[1].body, "повтор — та же запись");
    assert_eq!(f.connections.load(Ordering::SeqCst), 2);
}
