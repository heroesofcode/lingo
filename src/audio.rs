//! Audio capture through PipeWire (`pw-record`), one process per side of the conversation.
//!
//! The "eles" (them) stream is the monitor of the system output (what you hear in the call) and
//! "voce" (you) is the microphone. Both come out as 16-bit mono PCM at 24 kHz, the format the
//! Realtime API expects. Killing the process releases the microphone, which matters with Bluetooth
//! headsets: they stay in the call profile (HFP) while something reads the microphone.

use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use log::{info, warn};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

pub const RATE: u32 = 24_000;
pub const CHUNK_MS: u32 = 100;
pub const CHUNK_BYTES: usize = (RATE * 2 * CHUNK_MS / 1000) as usize;

/// RMS normalized to 0..1.
pub fn rms_level(pcm: &[u8]) -> f32 {
    let samples = pcm.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b) as f32);
    let (sum, count) = samples.fold((0.0f64, 0usize), |(sum, n), s| (sum + (s * s) as f64, n + 1));
    if count == 0 {
        return 0.0;
    }
    ((sum / count as f64).sqrt() / 32768.0) as f32
}

pub fn pw_record_command(sink_monitor: bool, target: &str, label: &str) -> Vec<String> {
    let mut props = vec![
        format!("media.name=\"Lingo {label}\""),
        "application.name=\"Lingo\"".into(),
        format!("node.name=lingo-{label}"),
    ];
    if sink_monitor {
        props.push("stream.capture.sink=true".into());
    }
    let mut cmd: Vec<String> =
        ["pw-record", "--rate", "24000", "--channels", "1", "--format", "s16"].map(String::from).into();
    if !target.is_empty() {
        cmd.extend(["--target".to_string(), target.to_string()]);
    }
    cmd.extend(["-P".to_string(), format!("{{ {} }}", props.join(" ")), "-".to_string()]);
    cmd
}

/// Reads 100 ms chunks from `pw-record` and restarts the process if it dies. Only returns if
/// `pw-record` is missing; to stop it, abort the task (the process dies with it).
pub async fn capture(
    label: &str,
    sink_monitor: bool,
    target: &str,
    mut on_chunk: impl FnMut(Vec<u8>, f32),
) -> std::io::Error {
    let mut backoff = Duration::from_millis(500);
    loop {
        let cmd = pw_record_command(sink_monitor, target, label);
        let spawned = Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return e,
            Err(e) => {
                warn!("captura {label}: {e}");
                tokio::time::sleep(backoff).await;
                continue;
            }
        };
        info!("captura {label} iniciada (pid {})", child.id().unwrap_or_default());
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout com pipe"));
        let mut chunk = vec![0u8; CHUNK_BYTES];
        while stdout.read_exact(&mut chunk).await.is_ok() {
            let level = rms_level(&chunk);
            on_chunk(chunk.clone(), level);
            backoff = Duration::from_millis(500);
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
        let mut err = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut err).await;
        }
        warn!("captura {label} terminou ({}); reiniciando", err.trim());
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

#[derive(Debug, PartialEq)]
pub struct Turn {
    pub chunks: Vec<Vec<u8>>,
    pub started: bool,
    pub commit: bool,
}

/// For the live models, which have no server-side VAD: sends only the stretches with voice (a minute
/// costs ~6x more) and decides when to close the sentence.
///
/// The sentence closes after `commit_ms` without voice, or after `long_commit_ms` once it has passed
/// `long_ms`, so a monologue does not become a single block. When the voice comes back, it resends
/// the last chunks so the start of the word is not cut off.
pub struct TurnGate {
    speech_level: f32,
    commit_ms: u32,
    long_ms: u32,
    long_commit_ms: u32,
    preroll_chunks: usize,
    open: bool,
    open_ms: u32,
    quiet_ms: u32,
    preroll: VecDeque<Vec<u8>>,
}

impl TurnGate {
    pub fn new(speech_level: f32, commit_ms: u32) -> TurnGate {
        TurnGate {
            speech_level,
            commit_ms,
            long_ms: 20_000,
            long_commit_ms: 300,
            preroll_chunks: 2,
            open: false,
            open_ms: 0,
            quiet_ms: 0,
            preroll: VecDeque::new(),
        }
    }

    /// A sentence is in progress (someone talking or in a short pause).
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn process(&mut self, pcm: Vec<u8>, level: f32) -> Turn {
        if level >= self.speech_level {
            let started = !self.open;
            let mut chunks: Vec<Vec<u8>> = if started { self.preroll.drain(..).collect() } else { Vec::new() };
            self.preroll.clear();
            chunks.push(pcm);
            self.open = true;
            self.quiet_ms = 0;
            self.open_ms += CHUNK_MS;
            return Turn { chunks, started, commit: false };
        }
        if !self.open {
            self.preroll.push_back(pcm);
            while self.preroll.len() > self.preroll_chunks {
                self.preroll.pop_front();
            }
            return Turn { chunks: Vec::new(), started: false, commit: false };
        }
        self.quiet_ms += CHUNK_MS;
        self.open_ms += CHUNK_MS;
        let limit = if self.open_ms >= self.long_ms { self.long_commit_ms } else { self.commit_ms };
        if self.quiet_ms < limit {
            return Turn { chunks: vec![pcm], started: false, commit: false };
        }
        self.open = false;
        self.open_ms = 0;
        self.quiet_ms = 0;
        Turn { chunks: vec![pcm], started: false, commit: true }
    }
}

/// Stops sending audio after a while of silence (nothing playing).
///
/// Before stopping, it lets a few seconds of silence through so the other end can close the sentence.
/// When sound returns, it resends the last chunks.
pub struct SilenceGate {
    floor: f32,
    hold_chunks: usize,
    preroll_chunks: usize,
    quiet: usize,
    preroll: VecDeque<Vec<u8>>,
}

impl Default for SilenceGate {
    fn default() -> Self {
        SilenceGate::new(1e-4, 25, 3)
    }
}

impl SilenceGate {
    pub fn new(floor: f32, hold_chunks: usize, preroll_chunks: usize) -> SilenceGate {
        SilenceGate { floor, hold_chunks, preroll_chunks, quiet: 0, preroll: VecDeque::new() }
    }

    pub fn process(&mut self, pcm: Vec<u8>, level: f32) -> Vec<Vec<u8>> {
        if level > self.floor {
            let mut out: Vec<Vec<u8>> =
                if self.quiet >= self.hold_chunks { self.preroll.drain(..).collect() } else { Vec::new() };
            self.preroll.clear();
            out.push(pcm);
            self.quiet = 0;
            return out;
        }
        self.quiet += 1;
        if self.quiet <= self.hold_chunks {
            return vec![pcm];
        }
        self.preroll.push_back(pcm);
        while self.preroll.len() > self.preroll_chunks {
            self.preroll.pop_front();
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOICE: f32 = 0.05;
    const QUIET: f32 = 0.0005;

    fn chunk(n: u8) -> Vec<u8> {
        vec![n]
    }

    fn turn(chunks: Vec<Vec<u8>>, started: bool, commit: bool) -> Turn {
        Turn { chunks, started, commit }
    }

    #[test]
    fn sends_nothing_before_speech_then_the_preroll() {
        let mut gate = TurnGate::new(0.003, 700);
        for i in 0..5 {
            assert_eq!(gate.process(chunk(i), QUIET), turn(vec![], false, false));
        }
        assert_eq!(gate.process(chunk(9), VOICE), turn(vec![chunk(3), chunk(4), chunk(9)], true, false));
        assert_eq!(gate.process(chunk(10), VOICE), turn(vec![chunk(10)], false, false));
    }

    #[test]
    fn commits_after_the_silence_and_stops_sending() {
        let mut gate = TurnGate::new(0.003, 300);
        gate.process(chunk(0), VOICE);
        assert_eq!(gate.process(chunk(1), QUIET), turn(vec![chunk(1)], false, false));
        assert_eq!(gate.process(chunk(2), QUIET), turn(vec![chunk(2)], false, false));
        assert_eq!(gate.process(chunk(3), QUIET), turn(vec![chunk(3)], false, true));
        assert_eq!(gate.process(chunk(4), QUIET), turn(vec![], false, false));
    }

    #[test]
    fn short_pause_keeps_the_same_sentence() {
        let mut gate = TurnGate::new(0.003, 700);
        gate.process(chunk(0), VOICE);
        for i in 0..4 {
            gate.process(chunk(i), QUIET);
        }
        assert_eq!(gate.process(chunk(5), VOICE), turn(vec![chunk(5)], false, false));
    }

    #[test]
    fn long_monologue_commits_on_a_short_pause() {
        let mut gate = TurnGate { long_ms: 1000, long_commit_ms: 200, ..TurnGate::new(0.003, 700) };
        for i in 0..10 {
            gate.process(chunk(i), VOICE);
        }
        gate.process(chunk(10), QUIET);
        assert!(gate.process(chunk(11), QUIET).commit);
    }

    #[test]
    fn rms_of_full_scale_square_wave_is_one() {
        let pcm: Vec<u8> = [i16::MAX, i16::MIN].repeat(100).iter().flat_map(|s| s.to_le_bytes()).collect();
        assert!((rms_level(&pcm) - 1.0).abs() < 0.001);
        assert_eq!(rms_level(&[]), 0.0);
    }
}
