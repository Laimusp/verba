//! Сетевой поток приложения: tokio на одном потоке, команды — каналом, итоги — каналом
//! контроллеру. Поток трея и хук сеть не ждут никогда (CLAUDE.md §2). Здесь же живёт
//! `codex app-server` (обновление входа Codex): его холодный старт до 20 с ждёт только диктовка.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use tokio::sync::mpsc;

use super::app_server::{AppServer, Timeouts};
use super::codex::{self, Codex, CodexSetup, Renewer};
use super::{detect_proxy_kind, probe_proxy, Answer, Failure, Hedge, Model, OpenRouter, Route, Warm};
use crate::config::{Engine, ProxyKind};

/// Чем распознавать запись.
pub enum Job {
    OpenRouter { key: String, model: String },
    Codex,
}

pub enum NetCmd {
    /// прогреть соединение движка (нажали хоткей); ключ — только OpenRouter
    Warm { engine: Engine, key: Option<String> },
    Transcribe { id: u64, job: Job, wav: Arc<Vec<u8>> },
    /// поменялся адрес или прокси OpenRouter
    SetRoute(Route),
    /// поменялись адрес, прокси или путь к codex движка Codex
    SetCodex(CodexSetup),
    /// проверить ключ OpenRouter (GET /api/v1/key)
    CheckKey { key: String },
    /// проверить движок Codex: 0,3 с тишины через весь путь
    CheckCodex,
    /// узнать вид прокси (строка его не назвала)
    DetectProxy { base_url: String, host: String, port: u16, user: Option<String>, pass: Option<String> },
    /// список моделей распознавания OpenRouter (окно настроек)
    ListModels,
    /// проверить прокси из окна настроек: вид (если не назван) и задержка до OpenRouter
    CheckProxy { base_url: String, host: String, port: u16, user: Option<String>, pass: Option<String>, kind: Option<ProxyKind> },
}

pub enum NetEvent {
    Warmed(Warm),
    Transcribed {
        id: u64,
        engine: Engine,
        result: Result<Answer, Failure>,
        sent_at: Instant,
        /// что было со входом Codex; у OpenRouter — пусто
        trace: codex::Trace,
    },
    ProxyDetected { host: String, port: u16, kind: Option<ProxyKind> },
    /// статус и мс до ответа
    KeyChecked(Result<(u16, u64), String>),
    CodexChecked(Result<codex::Checked, String>),
    RouteError(String),
    Models(Result<Vec<Model>, String>),
    /// итог проверки прокси: вид и задержка, мс; None — не ответил ни одним видом
    ProxyChecked { host: String, port: u16, result: Option<(ProxyKind, u64)> },
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
                // одно обновление входа на всё приложение — и через смену настроек Codex тоже
                let renewer = Renewer::new(AppServer::new(Timeouts::default()));
                let mut cx: Option<Arc<Codex>> = None;
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
                        NetCmd::SetCodex(setup) => {
                            if cx.as_ref().is_some_and(|c| *c.setup() == setup) {
                                continue;
                            }
                            match Codex::new(setup, renewer.clone()) {
                                Ok(c) => cx = Some(Arc::new(c)),
                                Err(e) => {
                                    cx = None;
                                    let _ = events.send(NetEvent::RouteError(e));
                                }
                            }
                        }
                        NetCmd::Warm { engine, key } => {
                            let ev = events.clone();
                            match engine {
                                Engine::OpenRouter => {
                                    let (Some(o), Some(key)) = (or.clone(), key) else { continue };
                                    tokio::spawn(async move {
                                        let w = o.warm(&key, Duration::from_secs(4)).await;
                                        let _ = ev.send(NetEvent::Warmed(w));
                                    });
                                }
                                Engine::Codex => {
                                    let Some(c) = cx.clone() else { continue };
                                    tokio::spawn(async move {
                                        let w = c.warm(Duration::from_secs(4)).await;
                                        let _ = ev.send(NetEvent::Warmed(w));
                                    });
                                }
                            }
                        }
                        NetCmd::Transcribe { id, job, wav } => {
                            let ev = events.clone();
                            let not_set = |engine| NetEvent::Transcribed {
                                id,
                                engine,
                                result: Err(Failure::Unreachable { why: "сеть не настроена".into(), attempts: vec![] }),
                                sent_at: Instant::now(),
                                trace: codex::Trace::default(),
                            };
                            match job {
                                Job::OpenRouter { key, model } => {
                                    let Some(o) = or.clone() else {
                                        let _ = ev.send(not_set(Engine::OpenRouter));
                                        continue;
                                    };
                                    tokio::spawn(async move {
                                        let sent_at = Instant::now();
                                        let result = o.transcribe(&key, &model, &wav, Hedge::default()).await;
                                        let _ = ev.send(NetEvent::Transcribed {
                                            id,
                                            engine: Engine::OpenRouter,
                                            result,
                                            sent_at,
                                            trace: codex::Trace::default(),
                                        });
                                    });
                                }
                                Job::Codex => {
                                    let Some(c) = cx.clone() else {
                                        let _ = ev.send(not_set(Engine::Codex));
                                        continue;
                                    };
                                    tokio::spawn(async move {
                                        let sent_at = Instant::now();
                                        let (result, trace) = c.transcribe(&wav, Hedge::default()).await;
                                        let _ = ev.send(NetEvent::Transcribed { id, engine: Engine::Codex, result, sent_at, trace });
                                    });
                                }
                            }
                        }
                        NetCmd::CheckKey { key } => {
                            let ev = events.clone();
                            let Some(o) = or.clone() else {
                                let _ = ev.send(NetEvent::KeyChecked(Err("сеть не настроена".into())));
                                continue;
                            };
                            tokio::spawn(async move {
                                let started = Instant::now();
                                let r = o.check_key(&key, Duration::from_secs(10)).await;
                                let ms = started.elapsed().as_millis() as u64;
                                let _ = ev.send(NetEvent::KeyChecked(r.map(|s| (s, ms))));
                            });
                        }
                        NetCmd::CheckCodex => {
                            let ev = events.clone();
                            let Some(c) = cx.clone() else {
                                let _ = ev.send(NetEvent::CodexChecked(Err("сеть не настроена".into())));
                                continue;
                            };
                            tokio::spawn(async move {
                                let r = c.check(Hedge::default()).await;
                                let _ = ev.send(NetEvent::CodexChecked(r));
                            });
                        }
                        NetCmd::ListModels => {
                            let ev = events.clone();
                            let Some(o) = or.clone() else {
                                let _ = ev.send(NetEvent::Models(Err("сеть не настроена".into())));
                                continue;
                            };
                            tokio::spawn(async move {
                                let _ = ev.send(NetEvent::Models(o.models(Duration::from_secs(15)).await));
                            });
                        }
                        NetCmd::CheckProxy { base_url, host, port, user, pass, kind } => {
                            let ev = events.clone();
                            tokio::spawn(async move {
                                let kinds = match kind {
                                    Some(k) => vec![k],
                                    None => vec![ProxyKind::Http, ProxyKind::Socks5, ProxyKind::Socks4],
                                };
                                let result = probe_proxy(
                                    &base_url,
                                    &host,
                                    port,
                                    user.as_deref(),
                                    pass.as_deref(),
                                    &kinds,
                                    Duration::from_secs(12),
                                )
                                .await;
                                let _ = ev.send(NetEvent::ProxyChecked { host, port, result });
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
                renewer.server().close().await;
            });
        })?;
        Ok(NetService { tx })
    }

    pub fn send(&self, cmd: NetCmd) {
        let _ = self.tx.send(cmd);
    }
}
