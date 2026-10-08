//! Configuração: padrões + `~/.config/lingo/config.toml` + chave da OpenAI.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("lingo")
}

pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("lingo")
}

pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("lingo")
}

pub fn key_file() -> PathBuf {
    config_dir().join("openai_key")
}

pub fn config_file() -> PathBuf {
    std::env::var_os("LINGO_CONFIG").map(PathBuf::from).unwrap_or_else(|| config_dir().join("config.toml"))
}

/// `~/x` → `/home/voce/x`.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home().join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

static CACHE_HOME_BEFORE: OnceLock<Option<OsString>> = OnceLock::new();

/// Cache de fontconfig só do Lingo: o Edge grava em `~/.cache/fontconfig` um cache em formato
/// novo que deixa o texto errado ou invisível em apps com fontconfig mais antigo.
pub fn isolate_font_cache() {
    let dir = std::env::var_os("LINGO_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".cache/lingo"));
    CACHE_HOME_BEFORE.set(std::env::var_os("XDG_CACHE_HOME")).ok();
    // SAFETY: roda no começo do main, antes de existir qualquer outra thread.
    unsafe { std::env::set_var("XDG_CACHE_HOME", dir) };
}

/// O `XDG_CACHE_HOME` de antes de `isolate_font_cache` (para devolvê-lo aos programas que o Lingo
/// abre). `None` quando o cache não foi isolado.
pub fn cache_home_before_isolation() -> Option<Option<OsString>> {
    CACHE_HOME_BEFORE.get().cloned()
}

/// `Translate`: a call é em outro idioma e a fala dos outros é traduzida.
/// `Native`: a call é no seu idioma; só transcreve, sem tradução.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Translate,
    Native,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "translate" => Some(Mode::Translate),
            "native" => Some(Mode::Native),
            _ => None,
        }
    }

    pub fn toggled(self) -> Mode {
        match self {
            Mode::Translate => Mode::Native,
            Mode::Native => Mode::Translate,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub languages: Languages,
    pub models: Models,
    pub audio: Audio,
    pub suggestions: Suggestions,
    pub ask: Ask,
    pub misc: Misc,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Languages {
    pub mode: String,
    /// o que os outros falam
    pub call: String,
    /// para onde traduzir; também o idioma do modo `native`
    pub translate_to: String,
    pub reply_in: String,
    pub my_names: Vec<String>,
    /// nomes, siglas e jargão que a transcrição deve acertar
    pub keywords: Vec<String>,
}

impl Default for Languages {
    fn default() -> Self {
        Languages {
            mode: "translate".into(),
            call: "en".into(),
            translate_to: "pt-BR".into(),
            reply_in: "en".into(),
            my_names: vec!["Pedro".into()],
            keywords: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Models {
    /// os outros: o texto aparece enquanto falam
    pub transcribe: String,
    /// gpt-live-transcribe: minimal | low | medium | high | xhigh
    pub transcribe_delay: String,
    /// você: só dá contexto às sugestões, não precisa ser ao vivo
    pub mic: String,
    /// gpt-realtime-translate traduz direto do áudio, ~2 s atrás da fala; um modelo de chat
    /// (gpt-5.4-mini) traduz frase a frase, quando cada uma termina
    pub translate: String,
    /// notas de expressões quando a tradução é ao vivo
    pub notes: String,
    pub suggest: String,
}

impl Default for Models {
    fn default() -> Self {
        Models {
            transcribe: "gpt-live-transcribe".into(),
            transcribe_delay: "high".into(),
            mic: "gpt-4o-transcribe".into(),
            translate: "gpt-realtime-translate".into(),
            notes: "gpt-5.4-mini".into(),
            suggest: "gpt-5.4-mini".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Audio {
    pub capture_mic: bool,
    /// vazio = saída padrão do sistema (monitor)
    pub they_target: String,
    /// vazio = microfone padrão
    pub mic_target: String,
    /// volume (RMS 0..1) que conta como voz nos modelos ao vivo
    pub speech_level: f32,
    /// silêncio que fecha a frase nos modelos ao vivo
    pub commit_ms: u32,
    /// idem nos modelos com VAD no servidor
    pub vad_silence_ms: u32,
    pub vad_threshold: f32,
}

impl Default for Audio {
    fn default() -> Self {
        Audio {
            capture_mic: true,
            they_target: String::new(),
            mic_target: String::new(),
            speech_level: 0.003,
            commit_ms: 700,
            vad_silence_ms: 500,
            vad_threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Suggestions {
    pub auto: bool,
    pub delay_s: f64,
    pub context_lines: usize,
}

impl Default for Suggestions {
    fn default() -> Self {
        Suggestions { auto: true, delay_s: 0.5, context_lines: 14 }
    }
}

/// Perguntar ao Claude Code, que tem a memória dele e o código dos projetos.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Ask {
    /// vazio = `claude` do PATH ou de ~/.local/bin
    pub command: String,
    /// pasta de onde ele roda; a memória do Claude Code é por pasta
    pub cwd: String,
}

impl Default for Ask {
    fn default() -> Self {
        Ask { command: String::new(), cwd: "~".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Misc {
    pub save_transcripts: bool,
    pub api_base: String,
    pub realtime_base: String,
}

impl Default for Misc {
    fn default() -> Self {
        Misc {
            save_transcripts: false,
            api_base: "https://api.openai.com/v1".into(),
            realtime_base: "wss://api.openai.com/v1".into(),
        }
    }
}

pub fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "es" => "Spanish",
        "pt" => "Portuguese",
        "pt-BR" => "Brazilian Portuguese",
        other => other,
    }
}

/// US$ por minuto de áudio enviado.
const PRICE_PER_MIN_USD: [(&str, f64); 6] = [
    ("gpt-live-transcribe", 0.017),
    ("gpt-realtime-whisper", 0.017),
    ("gpt-realtime-translate", 0.034),
    ("gpt-4o-transcribe", 0.006),
    ("gpt-4o-mini-transcribe", 0.003),
    ("whisper-1", 0.006),
];

pub fn price_per_min(model: &str) -> f64 {
    PRICE_PER_MIN_USD.iter().find(|(name, _)| model.starts_with(name)).map_or(0.0, |(_, price)| *price)
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn mode(&self) -> Mode {
        Mode::parse(&self.languages.mode).unwrap_or(Mode::Translate)
    }

    pub fn live_translation(&self) -> bool {
        self.models.translate.starts_with("gpt-realtime-translate")
    }

    /// Idiomas da transcrição de (eles, você) em cada modo, o principal primeiro.
    ///
    /// Na call no seu idioma o jargão vem em inglês ("o pod", "o deploy"); avisar o modelo
    /// disso derruba os erros nesses termos.
    pub fn languages_for(&self, mode: Mode) -> (Vec<String>, Vec<String>) {
        let lang = &self.languages;
        match mode {
            Mode::Native => (vec![lang.translate_to.clone(), lang.call.clone()], vec![lang.translate_to.clone()]),
            Mode::Translate => (vec![lang.call.clone()], vec![lang.reply_in.clone()]),
        }
    }

    pub fn vocabulary(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for word in self.languages.my_names.iter().chain(&self.languages.keywords) {
            if !word.is_empty() && !out.contains(word) {
                out.push(word.clone());
            }
        }
        out
    }

    pub fn mode_label(&self, mode: Mode) -> String {
        let short = |code: &str| code.split('-').next().unwrap_or(code).to_uppercase();
        match mode {
            Mode::Native => short(&self.languages.translate_to),
            Mode::Translate => format!("{}→{}", short(&self.languages.call), short(&self.languages.translate_to)),
        }
    }
}

pub fn load_api_key() -> Result<String, String> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Ok(key.trim().to_string());
    }
    let path = key_file();
    if let Ok(key) = std::fs::read_to_string(&path)
        && !key.trim().is_empty()
    {
        return Ok(key.trim().to_string());
    }
    Err(format!("Sem chave da OpenAI. Grave-a em {} (chmod 600) ou exporte OPENAI_API_KEY.", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mode_hints_english_jargon() {
        let mut cfg = Config::default();
        cfg.languages.keywords = vec!["Pedro".into(), "UAT".into()];
        assert_eq!(cfg.languages_for(Mode::Native), (vec!["pt-BR".into(), "en".into()], vec!["pt-BR".into()]));
        assert_eq!(cfg.languages_for(Mode::Translate), (vec!["en".into()], vec!["en".into()]));
        assert_eq!(cfg.vocabulary(), vec!["Pedro", "UAT"]);
    }

    #[test]
    fn toml_sections_override_only_what_they_set() {
        let cfg: Config = toml::from_str("[languages]\nkeywords = [\"pod\"]\n[audio]\ncommit_ms = 900\n").unwrap();
        assert_eq!(cfg.languages.keywords, vec!["pod"]);
        assert_eq!(cfg.languages.call, "en");
        assert_eq!(cfg.audio.commit_ms, 900);
        assert_eq!(cfg.models.transcribe, "gpt-live-transcribe");
    }
}
