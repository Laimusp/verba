//! Запись с микрофона (cpal → WASAPI) в своём потоке. Хоткей микрофон не ждёт (CLAUDE.md §5):
//! контроллер шлёт команду и идёт дальше, открытие устройства — здесь.
//!
//! Два режима (`microphone.keep_open`):
//! - по требованию (по умолчанию): поток звука открывается на нажатие и закрывается после — в
//!   простое микрофон не занят, CPU 0 %; каждое открытие берёт текущее устройство по умолчанию,
//!   поэтому смена и отключение микрофона переживаются сами;
//! - открыт всегда: звук идёт в кольцевой пре-буфер, на нажатие запись начинается с него —
//!   слова, сказанные одновременно с нажатием, не теряются.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};

use super::dsp::{self, Processed};
use crate::config::Microphone;

pub enum AudioCmd {
    Start { id: u64 },
    /// закончить запись через `tail_ms` и обработать
    Stop { id: u64, tail_ms: u32, trim_silence: bool },
    Cancel { id: u64 },
    Configure(Microphone),
}

pub enum AudioEvent {
    /// микрофон пишет; `open_ms` — от команды до готового потока
    Started { id: u64, open_ms: u64, device: String },
    Finished { id: u64, processed: Processed, encode_ms: u64, device_lost: bool },
    Failed { id: u64, words: String },
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_back_own_wav() {
        let w = super::dsp::wav(&[1, -2, 3], 16_000);
        let (rate, ch, data) = super::parse_wav(&w).unwrap();
        assert_eq!((rate, ch, data.len()), (16_000, 1, 6));
        assert!(super::parse_wav(b"RIFF0000WAVX").is_none());
    }
}

#[derive(Default)]
struct Capture {
    samples: Mutex<Vec<f32>>,
    ring: Mutex<VecDeque<f32>>,
    ring_cap: Mutex<usize>,
    recording: AtomicBool,
    lost: AtomicBool,
}

impl Capture {
    fn push(&self, mono: &[f32]) {
        if self.recording.load(Ordering::Relaxed) {
            if let Ok(mut s) = self.samples.lock() {
                s.extend_from_slice(mono);
            }
            return;
        }
        let cap = self.ring_cap.lock().map(|c| *c).unwrap_or(0);
        if cap == 0 {
            return;
        }
        if let Ok(mut r) = self.ring.lock() {
            r.extend(mono.iter().copied());
            let extra = r.len().saturating_sub(cap);
            r.drain(..extra);
        }
    }
}

struct Open {
    stream: cpal::Stream,
    rate: u32,
    device: String,
}

fn mic_words(e: impl std::fmt::Display) -> String {
    format!(
        "микрофон не открылся: {e} → проверь, что он подключён и что в «Параметры → Конфиденциальность → Микрофон» \
         разрешён доступ классическим приложениям"
    )
}

fn open(cfg: &Microphone, cap: &Arc<Capture>) -> Result<Open, String> {
    let host = cpal::default_host();
    let device = match &cfg.device {
        Some(want) => {
            let found = host
                .input_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.name().map(|n| n == *want).unwrap_or(false)));
            match found {
                Some(d) => Some(d),
                None => {
                    crate::warn!("микрофон «{want}» не найден — пишу с устройства по умолчанию");
                    host.default_input_device()
                }
            }
        }
        None => host.default_input_device(),
    }
    .ok_or_else(|| "микрофон не найден → подключи микрофон".to_string())?;
    let name = device.name().unwrap_or_else(|_| "без имени".into());
    let supported = device.default_input_config().map_err(mic_words)?;
    let channels = usize::from(supported.channels().max(1));
    let rate = supported.sample_rate().0;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    cap.lost.store(false, Ordering::Relaxed);
    let lost = cap.clone();
    let on_err = move |e: cpal::StreamError| {
        // устройство отключили или сменили посреди записи: записанное отдадим, следующее открытие возьмёт новое
        lost.lost.store(true, Ordering::Relaxed);
        crate::warn!("поток микрофона прервался: {e}");
    };
    macro_rules! build {
        ($t:ty, $conv:expr) => {{
            let c = cap.clone();
            let mut mono: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[$t], _: &cpal::InputCallbackInfo| {
                    mono.clear();
                    for frame in data.chunks(channels) {
                        let sum: f32 = frame.iter().map(|s| $conv(*s)).sum();
                        mono.push(sum / channels as f32);
                    }
                    c.push(&mono);
                },
                on_err,
                None,
            )
        }};
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build!(f32, |s: f32| s),
        cpal::SampleFormat::I16 => build!(i16, |s: i16| f32::from(s) / 32768.0),
        cpal::SampleFormat::I32 => build!(i32, |s: i32| s as f32 / 2_147_483_648.0),
        cpal::SampleFormat::U16 => build!(u16, |s: u16| (f32::from(s) - 32768.0) / 32768.0),
        cpal::SampleFormat::U8 => build!(u8, |s: u8| (f32::from(s) - 128.0) / 128.0),
        other => return Err(mic_words(format!("формат звука {other:?} не поддержан"))),
    }
    .map_err(mic_words)?;
    stream.play().map_err(mic_words)?;
    Ok(Open { stream, rate, device: name })
}

/// Тестовый вход: `VERBA_TEST_AUDIO=<файл.wav>` — запись берётся из файла (PCM 16 бит), а не с
/// микрофона. Нужен для проверки всей цепочки на живой системе без голоса; в обычной работе не задан.
fn test_audio() -> Option<(Vec<f32>, u32, String)> {
    let path = std::env::var_os("VERBA_TEST_AUDIO")?;
    let bytes = std::fs::read(&path).ok()?;
    let (rate, channels, data) = parse_wav(&bytes)?;
    let mono: Vec<f32> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
        .collect::<Vec<_>>()
        .chunks(usize::from(channels.max(1)))
        .map(|f| f.iter().sum::<f32>() / f.len() as f32)
        .collect();
    Some((mono, rate, format!("файл {}", std::path::Path::new(&path).display())))
}

/// WAV PCM 16 бит → (частота, каналы, данные).
pub fn parse_wav(b: &[u8]) -> Option<(u32, u16, &[u8])> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let (mut i, mut fmt) = (12usize, None);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().ok()?) as usize;
        let body = b.get(i + 8..(i + 8 + len).min(b.len()))?;
        if id == b"fmt " && body.len() >= 16 {
            let format = u16::from_le_bytes([body[0], body[1]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            if format != 1 || bits != 16 {
                return None;
            }
            fmt = Some((u32::from_le_bytes(body[4..8].try_into().ok()?), u16::from_le_bytes([body[2], body[3]])));
        } else if id == b"data" {
            let (rate, ch) = fmt?;
            return Some((rate, ch, body));
        }
        i += 8 + len + (len & 1);
    }
    None
}

pub fn spawn(events: Sender<AudioEvent>) -> std::io::Result<Sender<AudioCmd>> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new().name("verba-audio".into()).spawn(move || run(rx, events))?;
    Ok(tx)
}

fn run(rx: Receiver<AudioCmd>, events: Sender<AudioEvent>) {
    let cap = Arc::new(Capture::default());
    let mut cfg = Microphone::default();
    let mut open_now: Option<Open> = None;
    let mut active: Option<u64> = None;
    let mut test_rate: Option<u32> = None;

    let set_ring = |cfg: &Microphone, rate: u32| {
        let n = if cfg.keep_open { (rate as usize * cfg.prebuffer_ms as usize) / 1000 } else { 0 };
        if let Ok(mut c) = cap.ring_cap.lock() {
            *c = n;
        }
    };

    for cmd in rx {
        match cmd {
            AudioCmd::Configure(m) => {
                let changed = m != cfg;
                cfg = m;
                if changed && active.is_none() {
                    open_now = None;
                    if cfg.keep_open {
                        match open(&cfg, &cap) {
                            Ok(o) => {
                                set_ring(&cfg, o.rate);
                                crate::info!("микрофон открыт постоянно: {} ({} Гц), пре-буфер {} мс", o.device, o.rate, cfg.prebuffer_ms);
                                open_now = Some(o);
                            }
                            Err(w) => crate::warn!("{w}"),
                        }
                    }
                }
            }
            AudioCmd::Start { id } => {
                let t = Instant::now();
                if let Some((mono, rate, name)) = test_audio() {
                    *cap.samples.lock().unwrap_or_else(|p| p.into_inner()) = mono;
                    test_rate = Some(rate);
                    active = Some(id);
                    let _ = events.send(AudioEvent::Started { id, open_ms: t.elapsed().as_millis() as u64, device: name });
                    continue;
                }
                test_rate = None;
                if cap.lost.load(Ordering::Relaxed) || !cfg.keep_open {
                    open_now = None;
                }
                if open_now.is_none() {
                    match open(&cfg, &cap) {
                        Ok(o) => {
                            set_ring(&cfg, o.rate);
                            open_now = Some(o);
                        }
                        Err(words) => {
                            let _ = events.send(AudioEvent::Failed { id, words });
                            continue;
                        }
                    }
                }
                {
                    let mut s = cap.samples.lock().unwrap_or_else(|p| p.into_inner());
                    s.clear();
                    if cfg.keep_open {
                        let mut r = cap.ring.lock().unwrap_or_else(|p| p.into_inner());
                        s.extend(r.drain(..));
                    }
                }
                cap.recording.store(true, Ordering::Relaxed);
                active = Some(id);
                let device = open_now.as_ref().map(|o| o.device.clone()).unwrap_or_default();
                let _ = events.send(AudioEvent::Started { id, open_ms: t.elapsed().as_millis() as u64, device });
            }
            AudioCmd::Stop { id, tail_ms, trim_silence } => {
                if active != Some(id) {
                    continue;
                }
                // хвост: последний слог договаривается после отпускания
                if tail_ms > 0 {
                    std::thread::sleep(Duration::from_millis(u64::from(tail_ms)));
                }
                cap.recording.store(false, Ordering::Relaxed);
                active = None;
                let samples = std::mem::take(&mut *cap.samples.lock().unwrap_or_else(|p| p.into_inner()));
                let rate = test_rate.take().or(open_now.as_ref().map(|o| o.rate)).unwrap_or(48_000);
                let device_lost = cap.lost.load(Ordering::Relaxed);
                if !cfg.keep_open {
                    if let Some(o) = open_now.take() {
                        let _ = o.stream.pause();
                    }
                }
                // ресемплинг и VAD — в отдельном потоке: следующее нажатие не ждёт обработки этой записи
                let ev = events.clone();
                let _ = std::thread::Builder::new().name("verba-encode".into()).spawn(move || {
                    let t = Instant::now();
                    let processed = dsp::process(&samples, rate, trim_silence);
                    let encode_ms = t.elapsed().as_millis() as u64;
                    let _ = ev.send(AudioEvent::Finished { id, processed, encode_ms, device_lost });
                });
            }
            AudioCmd::Cancel { id } => {
                if active != Some(id) {
                    continue;
                }
                cap.recording.store(false, Ordering::Relaxed);
                active = None;
                cap.samples.lock().unwrap_or_else(|p| p.into_inner()).clear();
                if !cfg.keep_open {
                    open_now = None;
                }
            }
        }
    }
}
