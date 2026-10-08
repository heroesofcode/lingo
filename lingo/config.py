"""Configuração: defaults + ~/.config/lingo/config.toml + chave da OpenAI."""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "lingo"
STATE_DIR = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state")) / "lingo"
DATA_DIR = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "lingo"
CONFIG_FILE = Path(os.environ.get("LINGO_CONFIG", CONFIG_DIR / "config.toml"))
KEY_FILE = CONFIG_DIR / "openai_key"

LANGUAGE_NAMES = {
    "en": "English",
    "es": "Spanish",
    "pt": "Portuguese",
    "pt-BR": "Brazilian Portuguese",
}

# "translate": a call é em outro idioma e a fala dos outros é traduzida.
# "native": a call é no seu idioma; só transcreve, sem tradução.
MODES = ("translate", "native")

# US$ por minuto de áudio enviado
PRICE_PER_MIN_USD = {
    "gpt-live-transcribe": 0.017,
    "gpt-realtime-whisper": 0.017,
    "gpt-realtime-translate": 0.034,
    "gpt-4o-transcribe": 0.006,
    "gpt-4o-mini-transcribe": 0.003,
    "whisper-1": 0.006,
}


def price_per_min(model: str) -> float:
    return next((p for name, p in PRICE_PER_MIN_USD.items() if model.startswith(name)), 0.0)


@dataclass
class Config:
    # Idiomas
    mode: str = "translate"
    call_language: str = "en"  # o que os outros falam
    translate_to: str = "pt-BR"  # para onde traduzir; também o idioma do modo "native"
    reply_language: str = "en"  # em que idioma você responde
    my_names: list[str] = field(default_factory=lambda: ["Pedro"])
    keywords: list[str] = field(default_factory=list)  # nomes, siglas e jargão que a transcrição deve acertar

    # Modelos
    transcribe_model: str = "gpt-live-transcribe"  # os outros: texto aparece enquanto falam
    transcribe_delay: str = "high"  # gpt-live-transcribe: minimal | low | medium | high | xhigh
    mic_model: str = "gpt-4o-transcribe"  # você: só dá contexto, não precisa ser ao vivo
    # gpt-realtime-translate traduz direto do áudio, ~2 s atrás da fala, num parágrafo por bloco;
    # um modelo de chat (gpt-5.4-mini) traduz frase a frase, quando cada uma termina.
    translate_model: str = "gpt-realtime-translate"
    notes_model: str = "gpt-5.4-mini"  # notas de expressões quando a tradução é ao vivo
    suggest_model: str = "gpt-5.4-mini"

    # Áudio
    capture_mic: bool = True
    they_target: str = ""  # vazio = saída padrão do sistema (monitor)
    mic_target: str = ""  # vazio = microfone padrão
    speech_level: float = 0.003  # volume (RMS 0..1) que conta como voz nos modelos ao vivo
    commit_ms: int = 700  # silêncio que fecha a frase nos modelos ao vivo
    vad_silence_ms: int = 500  # idem nos modelos com VAD no servidor
    vad_threshold: float = 0.5

    # Sugestões
    auto_suggest: bool = True
    suggest_delay_s: float = 0.5
    context_lines: int = 14

    # Perguntar ao Claude (Claude Code, com a memória dele e o código dos projetos)
    ask_command: str = ""  # vazio = `claude` do PATH ou de ~/.local/bin
    ask_cwd: str = "~"  # pasta de onde ele roda; a memória do Claude Code é por pasta

    # Outros
    save_transcripts: bool = False
    api_base: str = "https://api.openai.com/v1"
    realtime_base: str = "wss://api.openai.com/v1"

    def language_name(self, code: str) -> str:
        return LANGUAGE_NAMES.get(code, code)

    @property
    def live_translation(self) -> bool:
        return self.translate_model.startswith("gpt-realtime-translate")

    def languages(self, mode: str) -> tuple[list[str], list[str]]:
        """Idiomas da transcrição de (eles, você) em cada modo, o principal primeiro.

        Na call no seu idioma o jargão vem em inglês ("o pod", "o deploy"); avisar o modelo
        disso derruba os erros nesses termos.
        """
        if mode == "native":
            return [self.translate_to, self.call_language], [self.translate_to]
        return [self.call_language], [self.reply_language]

    def vocabulary(self) -> list[str]:
        return list(dict.fromkeys(w for w in [*self.my_names, *self.keywords] if w))

    def mode_label(self, mode: str) -> str:
        short = lambda code: code.split("-")[0].upper()
        if mode == "native":
            return short(self.translate_to)
        return f"{short(self.call_language)}→{short(self.translate_to)}"


_TOML_MAP = {
    "languages": {"mode": "mode", "call": "call_language", "translate_to": "translate_to",
                  "reply_in": "reply_language", "my_names": "my_names", "keywords": "keywords"},
    "models": {"transcribe": "transcribe_model", "transcribe_delay": "transcribe_delay", "mic": "mic_model",
               "translate": "translate_model", "notes": "notes_model", "suggest": "suggest_model"},
    "audio": {"capture_mic": "capture_mic", "they_target": "they_target", "mic_target": "mic_target",
              "speech_level": "speech_level", "commit_ms": "commit_ms",
              "vad_silence_ms": "vad_silence_ms", "vad_threshold": "vad_threshold"},
    "suggestions": {"auto": "auto_suggest", "delay_s": "suggest_delay_s", "context_lines": "context_lines"},
    "ask": {"command": "ask_command", "cwd": "ask_cwd"},
    "misc": {"save_transcripts": "save_transcripts", "api_base": "api_base", "realtime_base": "realtime_base"},
}


def load_config(path: Path = CONFIG_FILE) -> Config:
    cfg = Config()
    if not path.exists():
        return cfg
    with path.open("rb") as fh:
        data = tomllib.load(fh)
    for section, keys in _TOML_MAP.items():
        for toml_key, attr in keys.items():
            value = data.get(section, {}).get(toml_key)
            if value is not None:
                setattr(cfg, attr, value)
    if cfg.mode not in MODES:
        cfg.mode = "translate"
    return cfg


class MissingKeyError(RuntimeError):
    pass


def load_api_key() -> str:
    key = os.environ.get("OPENAI_API_KEY", "").strip()
    if key:
        return key
    if KEY_FILE.exists():
        key = KEY_FILE.read_text().strip()
        if key:
            return key
    raise MissingKeyError(
        f"Sem chave da OpenAI. Grave-a em {KEY_FILE} (chmod 600) ou exporte OPENAI_API_KEY."
    )
