//! Сеть против фейкового OpenRouter на 127.0.0.1 (CLAUDE.md «Проверка»: не живые сервисы).
//! Сценарии: зависшее соединение, обрыв, 401/402/404/429/5xx, медленный ответ, keep-alive.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use verba::net::{Failure, Hedge, OpenRouter, Outcome, Route};

#[derive(Clone, Debug)]
enum Act {
    /// прочитать запрос и молчать, держа соединение
    Hang,
    /// прочитать запрос и закрыть соединение
    Drop,
    /// ответить через `delay_ms`
    Reply { status: u16, body: &'static str, delay_ms: u64 },
}

fn ok(text: &'static str) -> Act {
    Act::Reply { status: 200, body: text, delay_ms: 0 }
}

struct Fake {
    url: String,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    paths: Arc<Mutex<Vec<String>>>,
}

async fn read_request(s: &mut TcpStream) -> Option<String> {
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
    let len = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    head.lines().next().map(|l| l.split_whitespace().nth(1).unwrap_or("").to_string())
}

async fn fake(script: Vec<Act>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let paths = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(script);
    let (c, r, p) = (connections.clone(), requests.clone(), paths.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            c.fetch_add(1, Ordering::SeqCst);
            let (script, r, p) = (script.clone(), r.clone(), p.clone());
            tokio::spawn(async move {
                while let Some(path) = read_request(&mut sock).await {
                    let i = r.fetch_add(1, Ordering::SeqCst);
                    p.lock().unwrap().push(path);
                    match script.get(i).cloned().unwrap_or(ok(r#"{"text":"лишний"}"#)) {
                        Act::Hang => {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                            return;
                        }
                        Act::Drop => return,
                        Act::Reply { status, body, delay_ms } => {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            let resp = format!(
                                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
                                body.len()
                            );
                            if sock.write_all(resp.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    Fake { url, connections, requests, paths }
}

fn client(f: &Fake) -> OpenRouter {
    OpenRouter::new(Route { base_url: f.url.clone(), proxy: None }).unwrap()
}

fn hedge(base_ms: u64) -> Hedge {
    Hedge { base_ms, per_mb_ms: 0, tries: 3, min_overall_ms: 10_000 }
}

const WAV: &[u8] = b"RIFF....WAVEfmt ";

#[tokio::test]
async fn answers_on_first_send() {
    let f = fake(vec![ok(r#"{"text":" привет "}"#)]).await;
    let a = client(&f).transcribe("k", "m/x", WAV, hedge(2000)).await.unwrap();
    assert_eq!(a.status, 200);
    assert_eq!(a.text().as_deref(), Some("привет"));
    assert_eq!(a.winner, 0);
    assert_eq!(a.attempts.len(), 1);
    assert_eq!(f.paths.lock().unwrap()[0], "/api/v1/audio/transcriptions");
}

#[tokio::test]
async fn hung_connection_is_replaced_by_a_new_one() {
    let f = fake(vec![Act::Hang, ok(r#"{"text":"второй"}"#)]).await;
    let started = Instant::now();
    let a = client(&f).transcribe("k", "m/x", WAV, hedge(300)).await.unwrap();
    let ms = started.elapsed().as_millis();
    assert_eq!(a.text().as_deref(), Some("второй"));
    assert_eq!(a.winner, 1);
    assert_eq!(a.attempts[0].outcome, Outcome::Dropped);
    assert!(a.attempts[1].fresh);
    assert!(a.attempts[1].connect_ms.is_some(), "повтор идёт по новому соединению");
    assert!((300..2000).contains(&ms), "{ms} мс");
    assert_eq!(f.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn broken_connection_goes_again_at_once() {
    let f = fake(vec![Act::Drop, ok(r#"{"text":"ок"}"#)]).await;
    let started = Instant::now();
    let a = client(&f).transcribe("k", "m/x", WAV, hedge(5000)).await.unwrap();
    assert_eq!(a.text().as_deref(), Some("ок"));
    assert_eq!(a.winner, 1);
    assert!(matches!(a.attempts[0].outcome, Outcome::Failed { .. }));
    assert!(started.elapsed() < Duration::from_secs(2), "обрыв не ждёт таймера повтора");
}

#[tokio::test]
async fn refusals_are_not_repeated() {
    for status in [401u16, 402, 404, 429] {
        let f = fake(vec![Act::Reply { status, body: r#"{"error":{"message":"нет"}}"#, delay_ms: 0 }]).await;
        let a = client(&f).transcribe("k", "m/x", WAV, hedge(200)).await.unwrap();
        assert_eq!(a.status, status);
        assert_eq!(a.attempts.len(), 1, "{status}: сервис ответил — повтор не нужен");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(f.requests.load(Ordering::SeqCst), 1, "{status}");
        let words = verba::net::words::refusal(a.status, &verba::net::words::or_message(a.json.as_ref()), "m/x");
        assert!(words.contains('→'), "{words}");
    }
}

#[tokio::test]
async fn server_error_goes_again() {
    let f = fake(vec![Act::Reply { status: 503, body: "{}", delay_ms: 0 }, ok(r#"{"text":"после 503"}"#)]).await;
    let a = client(&f).transcribe("k", "m/x", WAV, hedge(5000)).await.unwrap();
    assert_eq!(a.status, 200);
    assert_eq!(a.winner, 1);
}

#[tokio::test]
async fn server_error_three_times_is_the_answer() {
    let e = Act::Reply { status: 502, body: r#"{"error":{"message":"upstream"}}"#, delay_ms: 0 };
    let f = fake(vec![e.clone(), e.clone(), e]).await;
    let a = client(&f).transcribe("k", "m/x", WAV, hedge(5000)).await.unwrap();
    assert_eq!(a.status, 502);
    assert_eq!(a.attempts.len(), 3);
}

#[tokio::test]
async fn all_hung_times_out_with_three_sends() {
    let f = fake(vec![Act::Hang, Act::Hang, Act::Hang]).await;
    let h = Hedge { base_ms: 100, per_mb_ms: 0, tries: 3, min_overall_ms: 800 };
    let started = Instant::now();
    let e = client(&f).transcribe("k", "m/x", WAV, h).await.unwrap_err();
    assert!(matches!(e, Failure::Timeout { .. }), "{e:?}");
    assert_eq!(e.attempts().len(), 3);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(e.words(false).contains("вставить последнее"));
}

#[tokio::test]
async fn all_broken_is_unreachable() {
    let f = fake(vec![Act::Drop, Act::Drop, Act::Drop]).await;
    let e = client(&f).transcribe("k", "m/x", WAV, hedge(5000)).await.unwrap_err();
    assert!(matches!(e, Failure::Unreachable { .. }), "{e:?}");
    assert_eq!(e.attempts().len(), 3);
}

#[tokio::test]
async fn slow_first_answer_still_wins() {
    let f = fake(vec![
        Act::Reply { status: 200, body: r#"{"text":"первый"}"#, delay_ms: 500 },
        Act::Reply { status: 200, body: r#"{"text":"второй"}"#, delay_ms: 3000 },
        Act::Reply { status: 200, body: r#"{"text":"третий"}"#, delay_ms: 3000 },
    ])
    .await;
    // повторы в 300 и 600 мс; первый отвечает в 500 мс
    let a = client(&f).transcribe("k", "m/x", WAV, hedge(300)).await.unwrap();
    assert_eq!(a.text().as_deref(), Some("первый"));
    assert_eq!(a.winner, 0);
    assert_eq!(a.attempts.len(), 2, "медленный ответ — повод для второй отправки");
    assert_eq!(a.attempts[1].outcome, Outcome::Dropped);
}

#[tokio::test]
async fn warm_connection_is_reused() {
    let f = fake(vec![ok(r#"{"data":{}}"#), ok(r#"{"text":"по прогретому"}"#)]).await;
    let c = client(&f);
    let w = c.warm("k", Duration::from_secs(2)).await;
    assert_eq!(w.status, Some(200));
    assert!(w.connect_ms.is_some());
    let a = c.transcribe("k", "m/x", WAV, hedge(2000)).await.unwrap();
    assert_eq!(a.text().as_deref(), Some("по прогретому"));
    assert_eq!(a.attempts[0].connect_ms, None, "соединение прежнее");
    assert_eq!(f.connections.load(Ordering::SeqCst), 1);
    assert_eq!(f.paths.lock().unwrap()[0], "/api/v1/key");
}

#[tokio::test]
async fn hung_warm_replaces_the_client() {
    let f = fake(vec![Act::Hang, ok(r#"{"data":{}}"#), ok(r#"{"text":"после замены"}"#)]).await;
    let c = client(&f);
    let w = c.warm("k", Duration::from_millis(300)).await;
    assert!(w.replaced);
    assert_eq!(w.status, Some(200));
    let a = c.transcribe("k", "m/x", WAV, hedge(2000)).await.unwrap();
    assert_eq!(a.text().as_deref(), Some("после замены"));
    assert_eq!(a.attempts[0].connect_ms, None, "запись идёт по соединению второго прогрева");
    assert_eq!(f.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn winner_becomes_the_primary_client() {
    let f = fake(vec![Act::Hang, ok(r#"{"text":"1"}"#), ok(r#"{"text":"2"}"#)]).await;
    let c = client(&f);
    let a = c.transcribe("k", "m/x", WAV, hedge(200)).await.unwrap();
    assert_eq!(a.winner, 1);
    let b = c.transcribe("k", "m/x", WAV, hedge(2000)).await.unwrap();
    assert_eq!(b.text().as_deref(), Some("2"));
    assert_eq!(b.attempts[0].connect_ms, None, "вторая диктовка идёт по соединению победителя");
}

#[tokio::test]
async fn nothing_listening_is_unreachable_fast() {
    // порт, на котором никто не слушает
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let c = OpenRouter::new(Route { base_url: url, proxy: None }).unwrap();
    // Windows повторяет SYN после отказа: каждая отправка — ~2 с; три подряд — не ждут таймера повтора (5 с)
    let started = Instant::now();
    let e = c.transcribe("k", "m/x", WAV, hedge(5000)).await.unwrap_err();
    assert!(matches!(e, Failure::Unreachable { .. }), "{e:?}");
    assert_eq!(e.attempts().len(), 3);
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(e.words(true).contains("прокси"));
}
