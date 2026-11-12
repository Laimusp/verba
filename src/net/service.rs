//! Сетевой поток приложения: tokio на одном потоке, команды — каналом, итоги — каналом
//! контроллеру. Поток трея и хук сеть не ждут никогда (CLAUDE.md §2).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use tokio::sync::mpsc;

use super::{detect_proxy_kind, Answer, Failure, Hedge, OpenRouter, Route, Warm};
use crate::config::ProxyKind;

pub enum NetCmd {
    /// прогреть соединение (нажали хоткей)
    Warm { key: String },
    Transcribe { id: u64, key: String, model: String, wav: Arc<Vec<u8>> },
    /// поменялся адрес или прокси
    SetRoute(Route),
    /// проверить ключ (GET /api/v1/key)
    CheckKey { key: String },
    /// узнать вид прокси (строка его не назвала)
    DetectProxy { base_url: String, host: String, port: u16, user: Option<String>, pass: Option<String> },
}

pub enum NetEvent {
    Warmed(Warm),
    Transcribed { id: u64, model: String, result: Result<Answer, Failure>, sent_at: Instant },
    ProxyDetected { host: String, port: u16, kind: Option<ProxyKind> },
    KeyChecked(Result<u16, String>),
    RouteError(String),
}

#[derive(Clone)]
pub struct NetService {
    tx: mpsc::UnboundedSender<NetCmd>,
}

impl NetService {
    pub fn spawn(route: Route, events: Sender<NetEvent>) -> std::io::Result<NetService> {
        let (tx, mut rx) = mpsc::unbounded_channel::<NetCmd>();
        std::thread::Builder::new().name("verba-net".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = events.send(NetEvent::RouteError(format!("сеть не запустилась: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                let mut or: Option<Arc<OpenRouter>> = match OpenRouter::new(route) {
                    Ok(o) => Some(Arc::new(o)),
                    Err(e) => {
                        let _ = events.send(NetEvent::RouteError(e));
                        None
                    }
                };
                while let Some(cmd) = rx.recv().await {
                    match cmd {
                        NetCmd::SetRoute(route) => {
                            if or.as_ref().is_some_and(|o| *o.route() == route) {
                                continue;
                            }
                            match OpenRouter::new(route) {
                                Ok(o) => or = Some(Arc::new(o)),
                                Err(e) => {
                                    or = None;
                                    let _ = events.send(NetEvent::RouteError(e));
                                }
                            }
                        }
                        NetCmd::Warm { key } => {
                            let Some(o) = or.clone() else { continue };
                            let ev = events.clone();
                            tokio::spawn(async move {
                                let w = o.warm(&key, Duration::from_secs(4)).await;
                                let _ = ev.send(NetEvent::Warmed(w));
                            });
                        }
                        NetCmd::Transcribe { id, key, model, wav } => {
                            let ev = events.clone();
                            let Some(o) = or.clone() else {
                                let _ = ev.send(NetEvent::Transcribed {
                                    id,
                                    model,
                                    result: Err(Failure::Unreachable { why: "сеть не настроена".into(), attempts: vec![] }),
                                    sent_at: Instant::now(),
                                });
                                continue;
                            };
                            tokio::spawn(async move {
                                let sent_at = Instant::now();
                                let result = o.transcribe(&key, &model, &wav, Hedge::default()).await;
                                let _ = ev.send(NetEvent::Transcribed { id, model, result, sent_at });
                            });
                        }
                        NetCmd::CheckKey { key } => {
                            let ev = events.clone();
                            let Some(o) = or.clone() else {
                                let _ = ev.send(NetEvent::KeyChecked(Err("сеть не настроена".into())));
                                continue;
                            };
                            tokio::spawn(async move {
                                let r = o.check_key(&key, Duration::from_secs(10)).await;
                                let _ = ev.send(NetEvent::KeyChecked(r));
                            });
                        }
                        NetCmd::DetectProxy { base_url, host, port, user, pass } => {
                            let ev = events.clone();
                            tokio::spawn(async move {
                                let kind = detect_proxy_kind(
                                    &base_url,
                                    &host,
                                    port,
                                    user.as_deref(),
                                    pass.as_deref(),
                                    Duration::from_secs(12),
                                )
                                .await;
                                let _ = ev.send(NetEvent::ProxyDetected { host, port, kind });
                            });
                        }
                    }
                }
            });
        })?;
        Ok(NetService { tx })
    }

    pub fn send(&self, cmd: NetCmd) {
        let _ = self.tx.send(cmd);
    }
}
