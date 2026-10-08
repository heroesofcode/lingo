//! Orquestra captura, transcrição, tradução e sugestões.
//!
//! O motor é uma tarefa só: comandos da janela, eventos dos WebSockets e respostas dos modelos
//! chegam como mensagens na mesma fila e são tratados um de cada vez, então o estado não precisa
//! de locks. Tudo o que a janela precisa saber sai como `Event`.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use indexmap::IndexMap;
use log::{error, info, warn};
use serde_json::json;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::ask::{self, AskEvent};
use crate::audio::{self, SilenceGate, TurnGate};
use crate::config::{Config, Mode, data_dir, expand_home, language_name, price_per_min};
use crate::llm::Chat;
use crate::prompts;
use crate::realtime::{self, Item, Role, SendQueue, SessionArgs, Socket, SocketEvent, Status};
use crate::text::{self, ReplyOption};

const MAX_UTTERANCES: usize = 400;
const ECHO_WINDOW: Duration = Duration::from_secs(20);
const AUTO_MIN_INTERVAL: Duration = Duration::from_secs(3);
/// falas seguidas da mesma pessoa com menos que isso entre elas formam um bloco
const BLOCK_GAP: Duration = Duration::from_secs(20);
/// a tradução ao vivo só passa para a fala seguinte depois de uma pausa dela
const TR_SETTLE: Duration = Duration::from_millis(800);
/// 3 s de silêncio depois da fala; com menos, o tradutor engole as últimas palavras
const TRANSLATOR_HOLD_CHUNKS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Speaker {
    They,
    Me,
}

impl Speaker {
    pub fn label(self) -> &'static str {
        match self {
            Speaker::They => "THEY",
            Speaker::Me => "ME",
        }
    }
}

/// De onde vem um aviso de conexão: a transcrição de cada lado ou o tradutor ao vivo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    They,
    Me,
    Translator,
}

impl From<Speaker> for Origin {
    fn from(speaker: Speaker) -> Origin {
        match speaker {
            Speaker::They => Origin::They,
            Speaker::Me => Origin::Me,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggKind {
    Reply,
    Phrase,
}

/// O que a janela pede ao motor.
#[derive(Debug)]
pub enum Command {
    /// sugere respostas para a fala `uid` (ou a última dos outros)
    Suggest(Option<String>),
    /// pergunta ao Claude o texto digitado ou, sem ele, a fala `uid` (ou a última dos outros)
    Ask {
        question: String,
        uid: Option<String>,
    },
    /// "como digo isso?"
    Phrase(String),
    SetPaused(bool),
    SetMic(bool),
    SetMode(Mode),
    Clear,
}

/// O que o motor conta para a janela.
#[derive(Debug, Clone)]
pub enum Event {
    Levels { they: f32, me: f32 },
    Status { origin: Origin, status: Status, detail: String },
    UttStart { uid: String, speaker: Speaker, cont: bool },
    UttPartial { uid: String, text: String },
    UttFinal { uid: String, speaker: Speaker, text: String },
    UttRemove { uid: String },
    Translation { uid: String, text: String },
    Note { uid: String, note: String },
    SuggStart { req: u64, kind: SuggKind, title: String, subtitle: String, auto: bool },
    SuggOptions { req: u64, options: Vec<ReplyOption> },
    SuggDone { req: u64 },
    AskStart { req: u64, question: String },
    AskText { req: u64, text: String },
    AskProgress { req: u64, text: String },
    AskDone { req: u64 },
    Usage { minutes: f64, usd: f64 },
    Paused(bool),
    Mic(bool),
    Mode(Mode),
    Cleared,
    Error(String),
    Notice(String),
}

enum Msg {
    Command(Command),
    Socket(Origin, SocketEvent),
    SpeechStarted(Speaker),
    CaptureFailed(String),
    Segment { uid: String, seg: usize, raw: String, done: bool, error: Option<String> },
    Shutdown(oneshot::Sender<()>),
}

/// Fila de entrada do motor, entregue a `Engine::run`.
pub struct Inbox(mpsc::UnboundedReceiver<Msg>);

#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl EngineHandle {
    /// Um motor que não existe (sem chave da OpenAI): os comandos são ignorados.
    pub fn detached() -> EngineHandle {
        EngineHandle { tx: mpsc::unbounded_channel().0 }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Command(cmd));
    }

    pub async fn shutdown(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Msg::Shutdown(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

#[derive(Clone)]
struct Ui(async_channel::Sender<Event>);

impl Ui {
    fn send(&self, ev: Event) {
        let _ = self.0.try_send(ev);
    }
}

/// Pedaço de uma fala (uma frase, em geral) traduzido sozinho, assim que termina.
#[derive(Debug, Default)]
struct Segment {
    source: String,
    translation: String,
    note: String,
    done: bool,
}

#[derive(Debug)]
struct Utterance {
    uid: String,
    speaker: Speaker,
    started: Instant,
    started_wall: DateTime<Local>,
    text: String,
    is_final: bool,
    /// deltas como chegaram; os cortes para tradução são feitos sobre ele
    live: String,
    /// quanto de `live` já foi mandado traduzir
    cut: usize,
    segments: Vec<Segment>,
    translation: String,
    note: String,
    saved_translation: bool,
    /// palavras que a fala tinha quando a sugestão saiu no "?"
    asked_words: usize,
    /// quando chegou o último texto
    updated: Instant,
}

#[derive(Default)]
struct Usage {
    minutes: f64,
    usd: f64,
}

impl Usage {
    fn add(&mut self, socket: &Socket) {
        let minutes = socket.seconds_sent() / 60.0;
        self.minutes += minutes;
        self.usd += minutes * price_per_min(&socket.model);
    }
}

enum Gate {
    Turn(TurnGate),
    Silence(SilenceGate),
    Pass,
}

struct TranslatorFeed {
    gate: SilenceGate,
    queue: Arc<SendQueue>,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Um lado da conversa: a captura, a transcrição e (para os outros) o tradutor ao vivo.
struct Side {
    args: SessionArgs,
    transcriber: Socket,
    translator: Option<Socket>,
    /// compartilhado com a tarefa de captura, que manda o áudio também para o tradutor
    translator_feed: Arc<Mutex<Option<TranslatorFeed>>>,
    /// maior volume desde a última leitura, em bits de f32 (para f32 ≥ 0 a ordem dos bits é a dos números)
    level: Arc<AtomicU32>,
    /// há uma frase em andamento (TurnGate aberto)
    speaking: Arc<AtomicBool>,
    _capture: AbortOnDrop,
}

pub struct Engine {
    cfg: Arc<Config>,
    api_key: String,
    ui: Ui,
    inbox: mpsc::UnboundedSender<Msg>,
    chat: Chat,
    utts: IndexMap<String, Utterance>,
    mode: Mode,
    paused: bool,
    mic_enabled: bool,
    sides: HashMap<Speaker, Side>,
    usage: Usage,
    ticks: u64,
    translate_slots: Arc<Semaphore>,
    suggest_task: Option<JoinHandle<()>>,
    ask_task: Option<JoinHandle<()>>,
    next_req: u64,
    auto_target: Option<String>,
    auto_deadline: Option<Instant>,
    last_auto: Option<Instant>,
    // tradução ao vivo: o texto corrido é repartido entre as falas dos outros
    tr_uid: Option<String>,
    tr_pending: String,
    tr_last: Option<Instant>,
    transcript: Option<PathBuf>,
    /// nos testes: guarda o texto das falas que receberiam sugestão em vez de chamar o modelo
    record: Option<Vec<String>>,
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

impl Engine {
    pub fn new(cfg: Config, api_key: String, ui: async_channel::Sender<Event>) -> (Engine, EngineHandle, Inbox) {
        let (tx, rx) = mpsc::unbounded_channel();
        let transcript = cfg.misc.save_transcripts.then(|| {
            let dir = data_dir().join("sessions");
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("{}.md", Local::now().format("%Y-%m-%d_%H%M")))
        });
        let engine = Engine {
            chat: Chat::new(&api_key, &cfg.misc.api_base),
            mode: cfg.mode(),
            mic_enabled: cfg.audio.capture_mic,
            cfg: Arc::new(cfg),
            api_key,
            ui: Ui(ui),
            inbox: tx.clone(),
            utts: IndexMap::new(),
            paused: false,
            sides: HashMap::new(),
            usage: Usage::default(),
            ticks: 0,
            translate_slots: Arc::new(Semaphore::new(3)),
            suggest_task: None,
            ask_task: None,
            next_req: 0,
            auto_target: None,
            auto_deadline: None,
            last_auto: None,
            tr_uid: None,
            tr_pending: String::new(),
            tr_last: None,
            transcript,
            record: None,
        };
        (engine, EngineHandle { tx }, Inbox(rx))
    }

    pub async fn run(mut self, Inbox(mut inbox): Inbox) {
        self.start_listening();
        let mut meter = tokio::time::interval(Duration::from_millis(100));
        meter.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = inbox.recv() => match msg {
                    Some(Msg::Shutdown(done)) => {
                        self.shutdown();
                        let _ = done.send(());
                        return;
                    }
                    Some(msg) => self.handle(msg, Instant::now()),
                    None => return,
                },
                _ = meter.tick() => self.meter_tick(),
                _ = wait_until(self.auto_deadline) => self.fire_auto(Instant::now()),
            }
        }
    }

    fn translating(&self) -> bool {
        self.mode == Mode::Translate
    }

    fn live_translation(&self) -> bool {
        self.translating() && self.cfg.live_translation()
    }

    fn next_req(&mut self) -> u64 {
        self.next_req += 1;
        self.next_req
    }

    fn handle(&mut self, msg: Msg, now: Instant) {
        match msg {
            Msg::Command(cmd) => self.command(cmd),
            Msg::Socket(origin, ev) => self.socket_event(origin, ev, now),
            Msg::SpeechStarted(speaker) => self.on_speech_started(speaker),
            Msg::CaptureFailed(err) => self.ui.send(Event::Error(err)),
            Msg::Segment { uid, seg, raw, done, error } => self.on_segment(&uid, seg, &raw, done, error),
            Msg::Shutdown(_) => {}
        }
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Suggest(uid) => self.suggest(uid.as_deref()),
            Command::Ask { question, uid } => self.ask(&question, uid.as_deref()),
            Command::Phrase(draft) => self.phrase(&draft),
            Command::SetPaused(paused) => self.set_paused(paused),
            Command::SetMic(enabled) => self.set_mic(enabled),
            Command::SetMode(mode) => self.set_mode(mode),
            Command::Clear => self.clear(),
        }
    }

    fn socket_event(&mut self, origin: Origin, ev: SocketEvent, now: Instant) {
        let speaker = if origin == Origin::Me { Speaker::Me } else { Speaker::They };
        match ev {
            SocketEvent::Status(status, detail) => self.ui.send(Event::Status { origin, status, detail }),
            SocketEvent::SpeechStarted => self.on_speech_started(speaker),
            SocketEvent::Delta { item, delta } => self.on_delta(speaker, &item, &delta, now),
            SocketEvent::Completed { item, transcript } => self.on_completed(speaker, &item, &transcript, now),
            SocketEvent::Failed { item, error } => {
                warn!("transcrição falhou ({}): {error}", speaker.label());
                self.drop_utterance(&format!("{}:{item}", speaker.label()), now);
            }
            SocketEvent::Translated(delta) => self.on_live_translation(&delta, now),
        }
    }

    // ---- ciclo de vida ------------------------------------------------

    fn shutdown(&mut self) {
        self.stop_listening();
        for task in [self.ask_task.take(), self.suggest_task.take()].into_iter().flatten() {
            task.abort(); // mata o `claude` que estiver rodando
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.cancel_auto();
            self.stop_listening();
        } else {
            self.start_listening();
        }
        self.ui.send(Event::Paused(paused));
    }

    fn set_mic(&mut self, enabled: bool) {
        self.mic_enabled = enabled;
        if !self.paused {
            if enabled {
                self.start_side(Speaker::Me);
            } else {
                self.stop_side(Speaker::Me);
            }
        }
        self.ui.send(Event::Mic(enabled));
    }

    fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        self.cancel_auto();
        let (they, me) = self.cfg.languages_for(mode);
        for (speaker, side) in &self.sides {
            let languages = if *speaker == Speaker::They { &they } else { &me };
            side.transcriber.set_update(realtime::session_update(&side.args, languages));
        }
        if self.sides.contains_key(&Speaker::They) {
            if self.live_translation() {
                self.start_translator();
            } else {
                self.stop_translator();
            }
        }
        self.ui.send(Event::Mode(mode));
    }

    fn clear(&mut self) {
        self.utts.clear();
        self.cancel_auto();
        self.tr_uid = None;
        self.tr_pending.clear();
        self.ui.send(Event::Cleared);
    }

    fn start_listening(&mut self) {
        self.start_side(Speaker::They);
        if self.mic_enabled {
            self.start_side(Speaker::Me);
        }
    }

    fn stop_listening(&mut self) {
        self.stop_side(Speaker::They);
        self.stop_side(Speaker::Me);
    }

    fn start_side(&mut self, speaker: Speaker) {
        if self.sides.contains_key(&speaker) {
            return;
        }
        let cfg = Arc::clone(&self.cfg);
        let they = speaker == Speaker::They;
        let (they_langs, me_langs) = cfg.languages_for(self.mode);
        let model = if they { cfg.models.transcribe.clone() } else { cfg.models.mic.clone() };
        let args = SessionArgs {
            model: model.clone(),
            keywords: cfg.vocabulary(),
            delay: cfg.models.transcribe_delay.clone(),
            silence_ms: cfg.audio.vad_silence_ms,
            threshold: cfg.audio.vad_threshold,
            noise_reduction: (!they).then_some("near_field"),
        };
        let update = realtime::session_update(&args, if they { &they_langs } else { &me_langs });
        let inbox = self.inbox.clone();
        let origin = Origin::from(speaker);
        let transcriber = Socket::spawn(
            Role::Transcriber,
            if they { "eles" } else { "voce" },
            realtime::transcription_url(&cfg.misc.realtime_base),
            self.api_key.clone(),
            model.clone(),
            update,
            move |ev| {
                let _ = inbox.send(Msg::Socket(origin, ev));
            },
        );

        let level = Arc::new(AtomicU32::new(0));
        let speaking = Arc::new(AtomicBool::new(false));
        let translator_feed: Arc<Mutex<Option<TranslatorFeed>>> = Arc::new(Mutex::new(None));
        let mut gate = if realtime::is_streaming(&model) {
            Gate::Turn(TurnGate::new(cfg.audio.speech_level, cfg.audio.commit_ms))
        } else if they {
            Gate::Silence(SilenceGate::default())
        } else {
            Gate::Pass
        };
        let on_chunk = {
            let (queue, level, speaking, feed, inbox) = (
                transcriber.queue.clone(),
                level.clone(),
                speaking.clone(),
                translator_feed.clone(),
                self.inbox.clone(),
            );
            move |pcm: Vec<u8>, volume: f32| {
                level.fetch_max(volume.to_bits(), Ordering::Relaxed);
                match &mut gate {
                    Gate::Turn(turns) => {
                        let turn = turns.process(pcm.clone(), volume);
                        if turn.started {
                            let _ = inbox.send(Msg::SpeechStarted(speaker));
                        }
                        for chunk in turn.chunks {
                            queue.push(Item::Audio(chunk));
                        }
                        if turn.commit {
                            queue.push(Item::Commit);
                        }
                        speaking.store(turns.is_open(), Ordering::Relaxed);
                    }
                    Gate::Silence(silence) => {
                        for chunk in silence.process(pcm.clone(), volume) {
                            queue.push(Item::Audio(chunk));
                        }
                    }
                    Gate::Pass => queue.push(Item::Audio(pcm.clone())),
                }
                if let Some(feed) = feed.lock().unwrap().as_mut() {
                    for chunk in feed.gate.process(pcm, volume) {
                        feed.queue.push(Item::Audio(chunk));
                    }
                }
            }
        };
        let label = if they { "eles" } else { "voce" };
        let target = if they { cfg.audio.they_target.clone() } else { cfg.audio.mic_target.clone() };
        let inbox = self.inbox.clone();
        let capture = tokio::spawn(async move {
            let err = audio::capture(label, they, &target, on_chunk).await;
            error!("captura {label}: {err}");
            let _ = inbox.send(Msg::CaptureFailed(format!("pw-record não funcionou: {err}")));
        });
        self.sides.insert(
            speaker,
            Side {
                args,
                transcriber,
                translator: None,
                translator_feed,
                level,
                speaking,
                _capture: AbortOnDrop(capture),
            },
        );
        if they && self.live_translation() {
            self.start_translator();
        }
    }

    fn start_translator(&mut self) {
        let Some(side) = self.sides.get_mut(&Speaker::They) else { return };
        if side.translator.is_some() {
            return;
        }
        let cfg = &self.cfg;
        let inbox = self.inbox.clone();
        let socket = Socket::spawn(
            Role::Translator,
            "traducao",
            realtime::translation_url(&cfg.misc.realtime_base, &cfg.models.translate),
            self.api_key.clone(),
            cfg.models.translate.clone(),
            realtime::translation_update(&cfg.languages.translate_to),
            move |ev| {
                let _ = inbox.send(Msg::Socket(Origin::Translator, ev));
            },
        );
        let gate = SilenceGate::new(cfg.audio.speech_level, TRANSLATOR_HOLD_CHUNKS, 2);
        *side.translator_feed.lock().unwrap() = Some(TranslatorFeed { gate, queue: socket.queue.clone() });
        side.translator = Some(socket);
    }

    fn stop_translator(&mut self) {
        if let Some(side) = self.sides.get_mut(&Speaker::They)
            && let Some(socket) = side.translator.take()
        {
            *side.translator_feed.lock().unwrap() = None;
            self.usage.add(&socket);
            self.ui.send(Event::Status { origin: Origin::Translator, status: Status::Stopped, detail: String::new() });
        }
    }

    fn stop_side(&mut self, speaker: Speaker) {
        let Some(mut side) = self.sides.remove(&speaker) else { return };
        if let Some(socket) = side.translator.take() {
            self.usage.add(&socket);
            self.ui.send(Event::Status { origin: Origin::Translator, status: Status::Stopped, detail: String::new() });
        }
        self.usage.add(&side.transcriber);
        drop(side); // aborta a captura (mata o pw-record) e fecha o WebSocket
        self.ui.send(Event::Status { origin: speaker.into(), status: Status::Stopped, detail: String::new() });
    }

    fn meter_tick(&mut self) {
        let level = |speaker| {
            self.sides.get(&speaker).map_or(0.0, |side: &Side| f32::from_bits(side.level.swap(0, Ordering::Relaxed)))
        };
        self.ui.send(Event::Levels { they: level(Speaker::They), me: level(Speaker::Me) });
        self.ticks += 1;
        if self.ticks.is_multiple_of(20) {
            let (mut minutes, mut usd) = (self.usage.minutes, self.usage.usd);
            for socket in
                self.sides.values().flat_map(|side| std::iter::once(&side.transcriber).chain(&side.translator))
            {
                let sent = socket.seconds_sent() / 60.0;
                minutes += sent;
                usd += sent * price_per_min(&socket.model);
            }
            self.ui.send(Event::Usage { minutes, usd });
        }
    }

    // ---- eventos da transcrição ---------------------------------------

    fn utterance(&mut self, speaker: Speaker, item: &str, now: Instant) -> String {
        let uid = format!("{}:{item}", speaker.label());
        if !self.utts.contains_key(&uid) {
            let cont = self
                .utts
                .last()
                .is_some_and(|(_, prev)| prev.speaker == speaker && now.duration_since(prev.updated) < BLOCK_GAP);
            let utt = Utterance {
                uid: uid.clone(),
                speaker,
                started: now,
                started_wall: Local::now(),
                text: String::new(),
                is_final: false,
                live: String::new(),
                cut: 0,
                segments: Vec::new(),
                translation: String::new(),
                note: String::new(),
                saved_translation: false,
                asked_words: 0,
                updated: now,
            };
            self.utts.insert(uid.clone(), utt);
            while self.utts.len() > MAX_UTTERANCES {
                self.utts.shift_remove_index(0);
            }
            self.ui.send(Event::UttStart { uid: uid.clone(), speaker, cont });
        }
        uid
    }

    fn on_speech_started(&mut self, speaker: Speaker) {
        // A linha só aparece com o primeiro texto, para não piscar com ruído.
        if speaker == Speaker::Me {
            self.cancel_auto();
        } else {
            // Ainda estão falando: espera terminarem antes de sugerir.
            self.auto_deadline = None;
        }
    }

    fn on_delta(&mut self, speaker: Speaker, item: &str, delta: &str, now: Instant) {
        let uid = self.utterance(speaker, item, now);
        let Some(utt) = self.utts.get_mut(&uid) else { return };
        if utt.is_final {
            return;
        }
        utt.live.push_str(delta);
        utt.text = text::collapse_spaces(&utt.live);
        utt.updated = now;
        let (line, asked) = (utt.text.clone(), utt.asked_words);
        self.ui.send(Event::UttPartial { uid: uid.clone(), text: line.clone() });
        if speaker != Speaker::They {
            return;
        }
        if self.translating() {
            self.cut(&uid, false);
        }
        let words = line.split_whitespace().count();
        if self.cfg.suggestions.auto && line.ends_with('?') && words > asked {
            // Sugere já no "?", sem esperar o silêncio que fecha a fala (~1,2 s a menos).
            if let Some(utt) = self.utts.get_mut(&uid) {
                utt.asked_words = words;
            }
            self.cancel_auto();
            self.last_auto = Some(now);
            self.start_suggest(&uid, true);
        }
    }

    fn drop_utterance(&mut self, uid: &str, now: Instant) {
        if self.utts.shift_remove(uid).is_some() {
            self.ui.send(Event::UttRemove { uid: uid.to_string() });
        }
        if let Some(target) = self.auto_target.clone()
            && self.auto_deadline.is_none()
        {
            self.arm_auto(target, now);
        }
    }

    fn on_completed(&mut self, speaker: Speaker, item: &str, transcript: &str, now: Instant) {
        let uid = self.utterance(speaker, item, now);
        let mut line = text::clean_transcript(transcript);
        if !line.is_empty() && speaker == Speaker::Me && self.is_echo(&line, now) {
            info!("descartado eco do microfone: {line:?}");
            line.clear();
        }
        if line.is_empty() {
            self.drop_utterance(&uid, now);
            return;
        }
        let Some(utt) = self.utts.get_mut(&uid) else { return };
        utt.text = line.clone();
        utt.is_final = true;
        utt.updated = now;
        let asked = utt.asked_words;
        self.ui.send(Event::UttFinal { uid: uid.clone(), speaker, text: line.clone() });
        self.save(&uid);
        if speaker != Speaker::They {
            return;
        }
        if self.translating() {
            if let Some(utt) = self.utts.get_mut(&uid)
                && utt.live.is_empty()
            {
                utt.live = line.clone();
            }
            self.cut(&uid, true);
            self.save_translation(&uid);
        }
        if asked > 0 && line.split_whitespace().count() <= asked {
            return; // já sugerido no "?" e nada foi dito depois
        }
        let auto = self.cfg.suggestions.auto;
        if auto && (text::is_question(&line) || text::mentions(&line, &self.cfg.languages.my_names)) {
            self.arm_auto(uid, now);
        } else if let Some(target) = self.auto_target.clone() {
            self.arm_auto(target, now);
        }
    }

    fn is_echo(&self, line: &str, now: Instant) -> bool {
        self.utts.values().rev().take(12).any(|u| {
            u.speaker == Speaker::They
                && u.is_final
                && now.duration_since(u.started) < ECHO_WINDOW
                && text::similar(line, &u.text) > 0.75
        })
    }

    // ---- gatilho automático -------------------------------------------

    fn arm_auto(&mut self, uid: String, now: Instant) {
        self.auto_target = Some(uid);
        let min_wait =
            self.last_auto.map_or(Duration::ZERO, |t| AUTO_MIN_INTERVAL.saturating_sub(now.duration_since(t)));
        let delay = Duration::from_secs_f64(self.cfg.suggestions.delay_s.max(0.0)).max(min_wait);
        self.auto_deadline = Some(now + delay);
    }

    fn fire_auto(&mut self, now: Instant) {
        self.auto_deadline = None;
        if self.sides.get(&Speaker::They).is_some_and(|side| side.speaking.load(Ordering::Relaxed)) {
            return; // emendaram outra frase; quando ela terminar o gatilho é armado de novo
        }
        let Some(target) = self.auto_target.take() else { return };
        if self.utts.contains_key(&target) && !self.paused {
            self.last_auto = Some(now);
            self.start_suggest(&target, true);
        }
    }

    fn cancel_auto(&mut self) {
        self.auto_deadline = None;
        self.auto_target = None;
    }

    // ---- tradução -----------------------------------------------------

    fn context(&self, upto: Option<&str>, n: usize) -> (Vec<(Speaker, String)>, Option<usize>) {
        let n = n.max(1);
        let items: Vec<&Utterance> =
            self.utts.values().filter(|u| u.is_final || (Some(u.uid.as_str()) == upto && !u.text.is_empty())).collect();
        let lines = |window: &[&Utterance]| window.iter().map(|u| (u.speaker, u.text.clone())).collect::<Vec<_>>();
        if let Some(upto) = upto
            && let Some(pos) = items.iter().position(|u| u.uid == upto)
        {
            let window = &items[(pos + 1).saturating_sub(n)..=pos];
            return (lines(window), Some(window.len() - 1));
        }
        (lines(&items[items.len().saturating_sub(n)..]), None)
    }

    /// Manda traduzir cada frase que terminou, sem esperar a pessoa parar de falar.
    fn cut(&mut self, uid: &str, is_final: bool) {
        let Some(utt) = self.utts.get_mut(uid) else { return };
        let (pieces, used) = text::take_sentences(&utt.live[utt.cut..], is_final);
        utt.cut += used;
        let first = utt.segments.len();
        utt.segments.extend(pieces.into_iter().map(|source| Segment { source, ..Segment::default() }));
        for seg in first..utt.segments.len() {
            self.start_segment(uid, seg);
        }
    }

    fn translation_context(&self, uid: &str, seg: usize) -> (Vec<(Speaker, String)>, usize) {
        let before: Vec<(Speaker, String)> = self
            .utts
            .values()
            .take_while(|u| u.uid != uid)
            .filter(|u| u.is_final)
            .map(|u| (u.speaker, u.text.clone()))
            .collect();
        let mut lines = before[before.len().saturating_sub(3)..].to_vec();
        let utt = &self.utts[uid];
        let prior: Vec<&str> = utt.segments[seg.saturating_sub(2)..seg].iter().map(|s| s.source.as_str()).collect();
        if !prior.is_empty() {
            lines.push((utt.speaker, prior.join(" ")));
        }
        lines.push((utt.speaker, utt.segments[seg].source.clone()));
        let idx = lines.len() - 1;
        (lines, idx)
    }

    /// Traduz o pedaço; com a tradução ao vivo ligada, só explica as expressões dele.
    fn start_segment(&mut self, uid: &str, seg: usize) {
        if self.record.is_some() {
            return;
        }
        let (lines, idx) = self.translation_context(uid, seg);
        let live = self.cfg.live_translation();
        let target = language_name(&self.cfg.languages.translate_to);
        let system = if live { prompts::notes_system(target) } else { prompts::translate_system(target) };
        let messages = json!([
            { "role": "system", "content": system },
            { "role": "user", "content": prompts::transcript_block(&lines, Some(idx)) },
        ]);
        let model = if live { self.cfg.models.notes.clone() } else { self.cfg.models.translate.clone() };
        let (chat, slots, inbox, uid) =
            (self.chat.clone(), self.translate_slots.clone(), self.inbox.clone(), uid.to_string());
        tokio::spawn(async move {
            let Ok(_slot) = slots.acquire_owned().await else { return };
            let mut raw = String::new();
            let result = chat
                .stream(&model, messages, 0.0, 220, |piece| {
                    raw.push_str(piece);
                    if !live {
                        let _ = inbox.send(Msg::Segment {
                            uid: uid.clone(),
                            seg,
                            raw: raw.clone(),
                            done: false,
                            error: None,
                        });
                    }
                })
                .await;
            let _ = inbox.send(Msg::Segment { uid, seg, raw, done: true, error: result.err() });
        });
    }

    fn on_segment(&mut self, uid: &str, seg: usize, raw: &str, done: bool, error: Option<String>) {
        let live = self.cfg.live_translation();
        if let Some(e) = error {
            warn!("tradução/notas falhou: {e}");
            if !live {
                // sem as notas dá para seguir; sem a tradução, não
                self.ui.send(Event::Error(format!("Tradução falhou: {e}")));
            }
        }
        let Some(segment) = self.utts.get_mut(uid).and_then(|utt| utt.segments.get_mut(seg)) else { return };
        if !live {
            let (translation, note) = text::split_translation(raw);
            segment.translation = translation;
            segment.note = text::filter_note(&note, &segment.source);
        } else if done {
            segment.note = text::filter_note(&text::parse_note(raw), &segment.source);
        }
        segment.done |= done;
        self.emit_translation(uid);
        if done {
            self.save_translation(uid);
        }
    }

    fn emit_translation(&mut self, uid: &str) {
        let live = self.cfg.live_translation();
        let Some(utt) = self.utts.get_mut(uid) else { return };
        if !live {
            let parts: Vec<&str> =
                utt.segments.iter().map(|s| s.translation.as_str()).filter(|t| !t.is_empty()).collect();
            utt.translation = parts.join(" ");
            self.ui.send(Event::Translation { uid: uid.to_string(), text: utt.translation.clone() });
        }
        let notes: Vec<&str> = utt.segments.iter().map(|s| s.note.as_str()).filter(|n| !n.is_empty()).collect();
        let source = if utt.live.is_empty() { &utt.text } else { &utt.live };
        utt.note = text::filter_note(&notes.join("; "), source);
        self.ui.send(Event::Note { uid: uid.to_string(), note: utt.note.clone() });
    }

    // ---- tradução ao vivo (gpt-realtime-translate) ---------------------

    /// Texto corrido da tradução; vai para a fala dos outros que está sendo traduzida.
    ///
    /// A API não marca fim de frase nem de fala, e o texto vem ~2 s atrás do original. A tradução
    /// só passa para a fala seguinte depois de uma pausa dela, então nunca adianta: no pior caso,
    /// o fim de uma frase aparece embaixo da fala seguinte.
    fn on_live_translation(&mut self, delta: &str, now: Instant) {
        let newest = self.utts.values().rev().find(|u| u.speaker == Speaker::They).map(|u| u.uid.clone());
        let mut current = self.tr_uid.clone().filter(|uid| self.utts.contains_key(uid));
        let settled = self.tr_last.is_none_or(|last| now.duration_since(last) >= TR_SETTLE);
        if let Some(newest) = newest
            && current.as_ref() != Some(&newest)
            && (current.is_none() || settled)
        {
            if let Some(previous) = &current {
                self.save_live_translation(previous);
            }
            let pending = std::mem::take(&mut self.tr_pending);
            if let Some(utt) = self.utts.get_mut(&newest) {
                utt.translation.insert_str(0, &pending);
            }
            self.tr_uid = Some(newest.clone());
            current = Some(newest);
        }
        self.tr_last = Some(now);
        let Some(uid) = current else {
            self.tr_pending.push_str(delta); // traduzido antes de a fala aparecer na tela
            return;
        };
        if let Some(utt) = self.utts.get_mut(&uid) {
            utt.translation.push_str(delta);
            self.ui.send(Event::Translation { text: text::collapse_spaces(&utt.translation), uid });
        }
    }

    // ---- sugestões e perguntas ----------------------------------------

    fn last_they_line(&self) -> Option<String> {
        self.utts.values().rev().find(|u| u.speaker == Speaker::They && !u.text.is_empty()).map(|u| u.uid.clone())
    }

    fn pick_target(&self, uid: Option<&str>) -> Option<String> {
        uid.filter(|u| self.utts.contains_key(*u)).map(str::to_string).or_else(|| self.last_they_line())
    }

    fn suggest(&mut self, uid: Option<&str>) {
        let Some(target) = self.pick_target(uid) else {
            self.ui.send(Event::Notice("Ainda não há fala dos outros para responder.".into()));
            return;
        };
        self.cancel_auto();
        self.start_suggest(&target, false);
    }

    fn start_suggest(&mut self, uid: &str, auto: bool) {
        let Some(utt) = self.utts.get(uid) else { return };
        if let Some(record) = &mut self.record {
            record.push(utt.text.clone());
            return;
        }
        let (title, translation) = (utt.text.clone(), utt.translation.clone());
        let (lines, idx) = self.context(Some(uid), self.cfg.suggestions.context_lines);
        let native = language_name(&self.cfg.languages.translate_to);
        let system = if self.translating() {
            prompts::suggest_system(language_name(&self.cfg.languages.reply_in), Some(native))
        } else {
            prompts::suggest_system(native, None)
        };
        let user = format!("Call transcript:\n{}", prompts::transcript_block(&lines, idx));
        // Com a tradução ao vivo, no "?" o português da pergunta ainda está chegando: título em inglês.
        let subtitle = if self.translating() && !self.live_translation() { translation } else { String::new() };
        self.stream_options(SuggKind::Reply, title, subtitle, auto, system, user, 0.7);
    }

    fn phrase(&mut self, draft: &str) {
        let draft = draft.trim();
        if draft.is_empty() {
            return;
        }
        let (lines, _) = self.context(None, 8);
        let system = prompts::phrase_system(
            language_name(&self.cfg.languages.reply_in),
            language_name(&self.cfg.languages.translate_to),
        );
        let user = format!("Call transcript (context):\n{}\n\nDraft: {draft}", prompts::transcript_block(&lines, None));
        self.stream_options(SuggKind::Phrase, draft.to_string(), String::new(), false, system, user, 0.6);
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_options(
        &mut self,
        kind: SuggKind,
        title: String,
        subtitle: String,
        auto: bool,
        system: String,
        user: String,
        temperature: f32,
    ) {
        if let Some(task) = self.suggest_task.take() {
            task.abort();
        }
        let req = self.next_req();
        let (ui, chat, model) = (self.ui.clone(), self.chat.clone(), self.cfg.models.suggest.clone());
        self.suggest_task = Some(tokio::spawn(async move {
            ui.send(Event::SuggStart { req, kind, title, subtitle, auto });
            let messages = json!([{ "role": "system", "content": system }, { "role": "user", "content": user }]);
            let mut raw = String::new();
            let result = chat
                .stream(&model, messages, temperature, 600, |piece| {
                    raw.push_str(piece);
                    ui.send(Event::SuggOptions { req, options: text::parse_options(&raw) });
                })
                .await;
            if let Err(e) = result {
                warn!("sugestão falhou: {e}");
                ui.send(Event::Error(format!("Sugestão falhou: {e}")));
            }
            ui.send(Event::SuggDone { req });
        }));
    }

    /// Pergunta ao Claude Code: o texto digitado ou, sem ele, a fala dos outros (a última ou `uid`).
    fn ask(&mut self, question: &str, uid: Option<&str>) {
        let question = question.trim().to_string();
        let target = if question.is_empty() {
            let Some(target) = self.pick_target(uid) else {
                self.ui.send(Event::Notice("Ainda não há fala dos outros para perguntar ao Claude.".into()));
                return;
            };
            Some(target)
        } else {
            None
        };
        let Some(command) = ask::find_claude(&self.cfg.ask.command) else {
            self.ui.send(Event::Error("Claude Code não encontrado; configure [ask] command.".into()));
            return;
        };
        if let Some(task) = self.ask_task.take() {
            task.abort();
        }
        let (lines, idx) = self.context(target.as_deref(), 8);
        let prompt = prompts::ask_prompt(&question, &prompts::transcript_block(&lines, idx));
        let shown = match &target {
            Some(uid) => self.utts[uid.as_str()].text.clone(),
            None => question,
        };
        let req = self.next_req();
        let (ui, cwd) = (self.ui.clone(), expand_home(&self.cfg.ask.cwd));
        self.ask_task = Some(tokio::spawn(async move {
            ui.send(Event::AskStart { req, question: shown });
            let command = [command.to_string_lossy().into_owned()];
            let result = ask::ask_claude(&command, &cwd, &prompt, |ev| match ev {
                AskEvent::Text(text) => ui.send(Event::AskText { req, text }),
                AskEvent::Progress(text) => ui.send(Event::AskProgress { req, text }),
            })
            .await;
            if let Err(e) = result {
                warn!("pergunta ao Claude falhou: {e}");
                ui.send(Event::Error(format!("Claude: {e}")));
            }
            ui.send(Event::AskDone { req });
        }));
    }

    // ---- histórico opcional -------------------------------------------

    fn append_transcript(&self, text: &str) {
        let Some(path) = &self.transcript else { return };
        let written =
            OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(text.as_bytes()));
        if let Err(e) = written {
            warn!("não deu para gravar a transcrição: {e}");
        }
    }

    fn save(&self, uid: &str) {
        let Some(utt) = self.utts.get(uid) else { return };
        let who = if utt.speaker == Speaker::They { "Eles" } else { "Você" };
        self.append_transcript(&format!("- `{}` **{who}:** {}\n", utt.started_wall.format("%H:%M:%S"), utt.text));
    }

    fn save_translation(&mut self, uid: &str) {
        if self.transcript.is_none() {
            return;
        }
        let live = self.cfg.live_translation();
        let Some(utt) = self.utts.get_mut(uid) else { return };
        if utt.saved_translation || !utt.is_final || utt.segments.is_empty() || !utt.segments.iter().all(|s| s.done) {
            return;
        }
        utt.saved_translation = true;
        let mut out = String::new();
        if !utt.translation.is_empty() && !live {
            out.push_str(&format!("  - _{}_\n", utt.translation)); // a ao vivo vai por save_live_translation
        }
        if !utt.note.is_empty() {
            out.push_str(&format!("  - 💡 {}\n", utt.note));
        }
        self.append_transcript(&out);
    }

    fn save_live_translation(&self, uid: &str) {
        let Some(utt) = self.utts.get(uid) else { return };
        let line = text::collapse_spaces(&utt.translation);
        if !line.is_empty() {
            self.append_transcript(&format!("  - _{line}_\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn engine(cfg: Config) -> (Engine, async_channel::Receiver<Event>) {
        let (tx, rx) = async_channel::unbounded();
        let (mut engine, _handle, _inbox) = Engine::new(cfg, "test-key".into(), tx);
        engine.record = Some(Vec::new());
        (engine, rx)
    }

    fn native() -> Config {
        let mut cfg = Config::default();
        cfg.languages.mode = "native".into();
        cfg
    }

    fn asked(engine: &Engine) -> Vec<String> {
        engine.record.clone().unwrap()
    }

    fn deltas(engine: &mut Engine, item: &str, pieces: &[&str], now: Instant) {
        for piece in pieces {
            engine.on_delta(Speaker::They, item, piece, now);
        }
    }

    #[test]
    fn suggests_as_soon_as_the_question_mark_arrives() {
        let (mut engine, _rx) = engine(native());
        let now = Instant::now();
        deltas(&mut engine, "i1", &[" Do", " you", " agree"], now);
        assert!(asked(&engine).is_empty());
        deltas(&mut engine, "i1", &["?"], now);
        assert_eq!(asked(&engine), ["Do you agree?"]);
        engine.on_completed(Speaker::They, "i1", "Do you agree?", now);
        assert!(engine.auto_deadline.is_none()); // nada novo depois do "?": não sugere de novo
    }

    #[test]
    fn suggests_again_when_they_keep_talking() {
        let (mut engine, _rx) = engine(native());
        let now = Instant::now();
        deltas(&mut engine, "i1", &[" Right", "?", " Because", " it", " failed", "."], now);
        assert_eq!(asked(&engine), ["Right?"]);
        engine.on_completed(Speaker::They, "i1", "Right? Because it failed.", now);
        assert!(engine.auto_deadline.is_some());
    }

    #[test]
    fn each_new_question_wins() {
        let (mut engine, _rx) = engine(native());
        deltas(&mut engine, "i1", &[" Right", "?", " Any", " ideas", "?"], Instant::now());
        assert_eq!(asked(&engine), ["Right?", "Right? Any ideas?"]);
    }

    /// A tradução ao vivo é um texto corrido; o motor decide embaixo de qual fala ela aparece.
    struct Live {
        engine: Engine,
        rx: async_channel::Receiver<Event>,
        now: Instant,
        shown: BTreeMap<String, String>,
    }

    impl Live {
        fn new() -> Live {
            let mut cfg = Config::default();
            cfg.suggestions.auto = false;
            let (engine, rx) = engine(cfg);
            Live { engine, rx, now: Instant::now(), shown: BTreeMap::new() }
        }

        fn say(&mut self, speaker: Speaker, item: &str, text: &str) {
            self.engine.on_delta(speaker, item, &format!(" {text}"), self.now);
            self.engine.on_completed(speaker, item, text, self.now);
        }

        fn translate(&mut self, text: &str, after: f64) {
            self.now += Duration::from_secs_f64(after);
            for word in text.split_whitespace() {
                self.engine.on_live_translation(&format!(" {word}"), self.now);
            }
        }

        fn shown(&mut self) -> BTreeMap<String, String> {
            while let Ok(ev) = self.rx.try_recv() {
                if let Event::Translation { uid, text } = ev {
                    self.shown.insert(uid, text);
                }
            }
            self.shown.iter().filter(|(_, text)| !text.is_empty()).map(|(k, v)| (k.clone(), v.clone())).collect()
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn each_line_gets_its_part_of_the_translation() {
        let mut t = Live::new();
        t.say(Speaker::They, "a", "We rolled out the retry logic.");
        t.translate("Lançamos a lógica de retentativa.", 0.3);
        t.now += Duration::from_secs(1);
        t.say(Speaker::They, "b", "The error rate dropped.");
        t.translate("A taxa de erro caiu.", 0.3);
        assert_eq!(
            t.shown(),
            map(&[("THEY:a", "Lançamos a lógica de retentativa."), ("THEY:b", "A taxa de erro caiu.")])
        );
    }

    #[test]
    fn translation_before_the_line_shows_up_is_kept() {
        let mut t = Live::new();
        t.translate("Bom dia,", 0.3);
        t.say(Speaker::They, "a", "Good morning, everyone.");
        t.translate("pessoal.", 0.1);
        assert_eq!(t.shown(), map(&[("THEY:a", "Bom dia, pessoal.")]));
    }

    #[test]
    fn next_line_waits_for_a_pause_in_the_translation() {
        let mut t = Live::new();
        t.say(Speaker::They, "a", "Some timeouts come from the vendor.");
        t.translate("Alguns timeouts vêm", 0.3);
        t.say(Speaker::Me, "m", "Okay.");
        t.say(Speaker::They, "c", "Pedro, any idea?");
        t.translate("do fornecedor.", 0.2); // ainda é o fim do bloco anterior
        t.translate("Pedro, alguma ideia?", 1.5); // depois de uma pausa: bloco novo
        assert_eq!(
            t.shown(),
            map(&[("THEY:a", "Alguns timeouts vêm do fornecedor."), ("THEY:c", "Pedro, alguma ideia?")])
        );
    }
}
