//! Real-time transcription and translation through OpenAI's Realtime API.
//!
//! One WebSocket per audio stream. With the live models (gpt-live-transcribe, gpt-realtime-whisper)
//! the text arrives word by word and the client closes the sentence (`TurnGate`); with the others,
//! the server detects the end of the sentence (server VAD) and only then transcribes. Reconnects on its own.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, Stream, StreamExt};
use log::{error, info, warn};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest, http::HeaderValue};

use crate::audio::RATE;

const STREAMING_MODELS: [&str; 2] = ["gpt-live-transcribe", "gpt-realtime-whisper"];
const QUEUE_CAP: usize = 60; // ~6 s of audio while reconnecting
const CREATED: [&str; 2] = ["session.created", "transcription_session.created"];
const UPDATED: [&str; 2] = ["session.updated", "transcription_session.updated"];

pub fn is_streaming(model: &str) -> bool {
    STREAMING_MODELS.iter().any(|m| model.starts_with(m))
}

/// The prompt of the server-VAD models must be in the call's language: the same instruction in
/// English in a Portuguese call doubled the errors in the tests.
fn prompt_rule(code: &str) -> (&'static str, &'static str) {
    match code {
        "pt" => {
            ("Escreva apenas as palavras que a pessoa falou; ruído, música e silêncio ficam sem texto.", "Vocabulário")
        }
        _ => ("Write only the words the speaker said; noise, music and silence get no text.", "Vocabulary"),
    }
}

pub fn transcribe_prompt(code: &str, keywords: &[String]) -> String {
    let (rule, label) = prompt_rule(code);
    if keywords.is_empty() { rule.to_string() } else { format!("{rule} {label}: {}.", keywords.join(", ")) }
}

#[derive(Debug, Clone)]
pub struct SessionArgs {
    pub model: String,
    pub keywords: Vec<String>,
    pub delay: String,
    pub silence_ms: u32,
    pub threshold: f32,
    pub noise_reduction: Option<&'static str>,
}

pub fn session_update(args: &SessionArgs, languages: &[String]) -> Value {
    let mut codes: Vec<String> = Vec::new();
    for lang in languages {
        let code = lang.split('-').next().unwrap_or("").to_lowercase();
        if !code.is_empty() && code != "auto" && !codes.contains(&code) {
            codes.push(code);
        }
    }
    let streaming = is_streaming(&args.model);
    let mut transcription = json!({ "model": args.model });
    if args.model.starts_with("gpt-live-transcribe") {
        if !codes.is_empty() {
            transcription["languages"] = json!(codes);
        }
        transcription["delay"] = json!(args.delay);
        if !args.keywords.is_empty() {
            transcription["keywords"] = json!(args.keywords);
        }
    } else if let Some(code) = codes.first() {
        transcription["language"] = json!(code);
    }
    if !streaming {
        let code = codes.first().map_or("", String::as_str);
        transcription["prompt"] = json!(transcribe_prompt(code, &args.keywords));
    }
    let turn_detection = if streaming {
        Value::Null
    } else {
        json!({
            "type": "server_vad",
            "threshold": args.threshold,
            "prefix_padding_ms": 300,
            "silence_duration_ms": args.silence_ms,
        })
    };
    let mut input = json!({
        "format": { "type": "audio/pcm", "rate": RATE },
        "transcription": transcription,
        "turn_detection": turn_detection,
    });
    if let Some(kind) = args.noise_reduction {
        input["noise_reduction"] = json!({ "type": kind });
    }
    json!({ "type": "session.update", "session": { "type": "transcription", "audio": { "input": input } } })
}

/// No "transcription": the original already comes from the other WebSocket, so it is not paid for twice.
pub fn translation_update(language: &str) -> Value {
    let code = language.split('-').next().unwrap_or(language).to_lowercase();
    json!({ "type": "session.update", "session": { "audio": { "output": { "language": code } } } })
}

pub fn transcription_url(base: &str) -> String {
    format!("{base}/realtime?intent=transcription")
}

pub fn translation_url(base: &str, model: &str) -> String {
    format!("{base}/realtime/translations?model={model}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Connecting,
    Listening,
    Reconnecting,
    Error,
    Stopped,
}

#[derive(Debug)]
pub enum SocketEvent {
    Status(Status, String),
    SpeechStarted,
    Delta { item: String, delta: String },
    Completed { item: String, transcript: String },
    Failed { item: String, error: String },
    Translated(String),
}

pub enum Item {
    Audio(Vec<u8>),
    /// closes the sentence (models without server-side VAD)
    Commit,
    Update(Value),
}

/// Send queue that drops the oldest item when it is full (connection down).
pub struct SendQueue {
    items: Mutex<VecDeque<Item>>,
    ready: Notify,
}

impl SendQueue {
    fn new() -> SendQueue {
        SendQueue { items: Mutex::new(VecDeque::new()), ready: Notify::new() }
    }

    pub fn push(&self, item: Item) {
        let mut items = self.items.lock().unwrap();
        if items.len() >= QUEUE_CAP {
            items.pop_front();
        }
        items.push_back(item);
        drop(items);
        self.ready.notify_one();
    }

    async fn pop(&self) -> Item {
        loop {
            if let Some(item) = self.items.lock().unwrap().pop_front() {
                return item;
            }
            self.ready.notified().await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Transcriber,
    /// gpt-realtime-translate: the translated text comes out ~2 s behind the speech, as running text that
    /// marks neither sentence nor utterance ends; the engine decides where it appears. The translated audio is ignored.
    Translator,
}

/// A connection open in the background. Dropping the `Socket` closes it.
pub struct Socket {
    pub model: String,
    pub queue: Arc<SendQueue>,
    bytes_sent: Arc<AtomicU64>,
    update: Arc<Mutex<Value>>,
    task: JoinHandle<()>,
}

impl Socket {
    pub fn spawn(
        role: Role,
        label: &'static str,
        url: String,
        api_key: String,
        model: String,
        update: Value,
        on_event: impl Fn(SocketEvent) + Send + Sync + 'static,
    ) -> Socket {
        let link = Link {
            role,
            label,
            url,
            api_key,
            queue: Arc::new(SendQueue::new()),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            update: Arc::new(Mutex::new(update)),
            on_event: Box::new(on_event),
        };
        let (queue, bytes_sent, update) = (link.queue.clone(), link.bytes_sent.clone(), link.update.clone());
        Socket { model, queue, bytes_sent, update, task: tokio::spawn(link.run()) }
    }

    pub fn seconds_sent(&self) -> f64 {
        self.bytes_sent.load(Ordering::Relaxed) as f64 / (RATE as f64 * 2.0)
    }

    /// Changes the configuration of the open session without reconnecting (it also applies to the next connection).
    pub fn set_update(&self, update: Value) {
        *self.update.lock().unwrap() = update.clone();
        self.queue.push(Item::Update(update));
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum Failure {
    /// no point retrying right away (invalid key, nonexistent model)
    Fatal(String),
    Http(u16),
    Net(String),
}

fn net(e: impl std::fmt::Display) -> Failure {
    Failure::Net(e.to_string())
}

struct Link {
    role: Role,
    label: &'static str,
    url: String,
    api_key: String,
    queue: Arc<SendQueue>,
    bytes_sent: Arc<AtomicU64>,
    update: Arc<Mutex<Value>>,
    on_event: Box<dyn Fn(SocketEvent) + Send + Sync>,
}

impl Link {
    fn status(&self, status: Status, detail: &str) {
        (self.on_event)(SocketEvent::Status(status, detail.to_string()));
    }

    async fn run(self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.session().await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(Failure::Fatal(msg)) => {
                    error!("{}: {msg}", self.label);
                    self.status(Status::Error, &msg);
                    sleep(Duration::from_secs(30)).await;
                    continue;
                }
                Err(Failure::Http(code @ (401 | 403))) => {
                    self.status(Status::Error, &format!("Chave da OpenAI recusada (HTTP {code})"));
                    sleep(Duration::from_secs(30)).await;
                    continue;
                }
                Err(Failure::Http(code)) => self.status(Status::Reconnecting, &format!("HTTP {code}")),
                Err(Failure::Net(e)) => {
                    warn!("{}: conexão caiu: {e}", self.label);
                    self.status(Status::Reconnecting, "conexão caiu");
                }
            }
            sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(20));
        }
    }

    async fn session(&self) -> Result<(), Failure> {
        self.status(Status::Connecting, "");
        let mut request = self.url.as_str().into_client_request().map_err(|e| Failure::Fatal(e.to_string()))?;
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.api_key))
            .map_err(|_| Failure::Fatal("a chave da OpenAI tem caracteres inválidos".into()))?;
        request.headers_mut().insert("Authorization", auth);
        let ws = match timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(request)).await {
            Err(_) => return Err(net("tempo esgotado ao conectar")),
            Ok(Err(tungstenite::Error::Http(response))) => {
                let body = response.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
                error!("{}: HTTP {} {}", self.label, response.status(), body.chars().take(200).collect::<String>());
                return Err(Failure::Http(response.status().as_u16()));
            }
            Ok(Err(e)) => return Err(net(e)),
            Ok(Ok((ws, _response))) => ws,
        };
        let (mut sink, mut stream) = ws.split();
        expect(&mut stream, &CREATED).await?;
        let update = self.update.lock().unwrap().to_string();
        sink.send(Message::text(update)).await.map_err(net)?;
        expect(&mut stream, &UPDATED).await?;
        info!("{}: sessão pronta", self.label);
        self.status(Status::Listening, "");

        let mut ping = tokio::time::interval(Duration::from_secs(20));
        ping.tick().await;
        let mut last_seen = Instant::now();
        loop {
            tokio::select! {
                frame = stream.next() => {
                    let Some(frame) = frame else { return Ok(()) };
                    last_seen = Instant::now();
                    match frame.map_err(net)? {
                        Message::Text(text) => self.handle(&text),
                        Message::Close(_) => return Ok(()),
                        _ => {}
                    }
                }
                item = self.queue.pop() => {
                    let text = match item {
                        Item::Audio(pcm) => {
                            self.bytes_sent.fetch_add(pcm.len() as u64, Ordering::Relaxed);
                            let append = match self.role {
                                Role::Transcriber => "input_audio_buffer.append",
                                Role::Translator => "session.input_audio_buffer.append",
                            };
                            json!({ "type": append, "audio": base64::engine::general_purpose::STANDARD.encode(pcm) }).to_string()
                        }
                        Item::Commit => json!({ "type": "input_audio_buffer.commit" }).to_string(),
                        Item::Update(update) => update.to_string(),
                    };
                    sink.send(Message::text(text)).await.map_err(net)?;
                }
                _ = ping.tick() => {
                    if last_seen.elapsed() > Duration::from_secs(45) {
                        return Err(net("o servidor parou de responder"));
                    }
                    sink.send(Message::Ping(Default::default())).await.map_err(net)?;
                }
            }
        }
    }

    fn handle(&self, text: &str) {
        let Ok(event) = serde_json::from_str::<Value>(text) else { return };
        let field = |key: &str| event[key].as_str().unwrap_or_default().to_string();
        let kind = event["type"].as_str().unwrap_or_default();
        let ev = match (self.role, kind) {
            (Role::Transcriber, "input_audio_buffer.speech_started") => SocketEvent::SpeechStarted,
            (Role::Transcriber, "conversation.item.input_audio_transcription.delta") => {
                SocketEvent::Delta { item: field("item_id"), delta: field("delta") }
            }
            (Role::Transcriber, "conversation.item.input_audio_transcription.completed") => {
                SocketEvent::Completed { item: field("item_id"), transcript: field("transcript") }
            }
            (Role::Transcriber, "conversation.item.input_audio_transcription.failed") => SocketEvent::Failed {
                item: field("item_id"),
                error: event["error"].to_string().chars().take(200).collect(),
            },
            (Role::Translator, "session.output_transcript.delta") => SocketEvent::Translated(field("delta")),
            (_, "error") => {
                if event["error"]["code"] != "input_audio_buffer_commit_empty" {
                    warn!("{}: erro do servidor: {}", self.label, event["error"]);
                }
                return;
            }
            _ => return,
        };
        (self.on_event)(ev);
    }
}

async fn expect<S>(stream: &mut S, types: &[&str]) -> Result<(), Failure>
where
    S: Stream<Item = Result<Message, tungstenite::Error>> + Unpin,
{
    loop {
        let frame = match timeout(Duration::from_secs(15), stream.next()).await {
            Err(_) => return Err(net("o servidor não respondeu")),
            Ok(None) => return Err(net("conexão fechada")),
            Ok(Some(frame)) => frame.map_err(net)?,
        };
        let Message::Text(text) = frame else { continue };
        let event: Value = serde_json::from_str(&text).unwrap_or_default();
        let kind = event["type"].as_str().unwrap_or_default();
        if kind == "error" {
            let msg = event["error"]["message"].as_str().unwrap_or("erro na sessão");
            return Err(Failure::Fatal(msg.to_string()));
        }
        if types.contains(&kind) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_input(model: &str, languages: &[&str], keywords: &[&str]) -> Value {
        let args = SessionArgs {
            model: model.into(),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            delay: "high".into(),
            silence_ms: 500,
            threshold: 0.5,
            noise_reduction: None,
        };
        let languages: Vec<String> = languages.iter().map(|l| l.to_string()).collect();
        session_update(&args, &languages)["session"]["audio"]["input"].clone()
    }

    #[test]
    fn live_model_gets_language_hints_and_keywords_without_server_vad() {
        let cfg = audio_input("gpt-live-transcribe", &["pt-BR", "en"], &["Event Hub", "UAT"]);
        assert!(cfg["turn_detection"].is_null());
        assert_eq!(
            cfg["transcription"],
            json!({"model": "gpt-live-transcribe", "languages": ["pt", "en"], "delay": "high", "keywords": ["Event Hub", "UAT"]})
        );
    }

    #[test]
    fn realtime_whisper_uses_one_language_without_prompt() {
        let cfg = audio_input("gpt-realtime-whisper", &["en"], &[]);
        assert!(cfg["turn_detection"].is_null());
        assert_eq!(cfg["transcription"], json!({"model": "gpt-realtime-whisper", "language": "en"}));
    }

    #[test]
    fn batch_model_prompt_is_in_the_call_language_with_the_vocabulary() {
        let cfg = audio_input("gpt-4o-transcribe", &["pt-BR"], &["Pedro", "Event Hub"]);
        assert_eq!(cfg["turn_detection"]["type"], "server_vad");
        assert_eq!(cfg["transcription"]["language"], "pt");
        let prompt = cfg["transcription"]["prompt"].as_str().unwrap();
        assert!(prompt.starts_with("Escreva apenas"));
        assert!(prompt.ends_with("Vocabulário: Pedro, Event Hub."));
        let english = audio_input("gpt-4o-transcribe", &["en"], &[]);
        assert!(english["transcription"]["prompt"].as_str().unwrap().starts_with("Write only"));
    }

    #[test]
    fn translation_only_translates_without_transcribing_again() {
        assert_eq!(
            translation_update("pt-BR"),
            json!({"type": "session.update", "session": {"audio": {"output": {"language": "pt"}}}})
        );
    }
}
