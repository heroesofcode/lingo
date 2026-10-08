"""Transcrição em tempo real pela Realtime API da OpenAI (sessão de transcrição).

Um WebSocket por lado da conversa. Nos modelos ao vivo (gpt-live-transcribe,
gpt-realtime-whisper) o texto chega palavra a palavra enquanto a pessoa fala e
quem fecha a frase é o cliente (TurnGate). Nos outros, o servidor detecta o fim
da frase (server VAD) e só então transcreve. Reconecta sozinho se a conexão cair.
"""

from __future__ import annotations

import asyncio
import base64
import json
import logging
from collections.abc import Callable
from dataclasses import dataclass

from websockets.asyncio.client import connect
from websockets.exceptions import ConnectionClosed, InvalidStatus

from .audio import RATE, SilenceGate, TurnGate

log = logging.getLogger(__name__)

STREAMING_MODELS = ("gpt-live-transcribe", "gpt-realtime-whisper")
COMMIT = None  # na fila de envio: fechar a frase

# O prompt dos modelos com VAD no servidor precisa estar no idioma da call: a mesma instrução
# em inglês numa call em português dobrou os erros nos testes.
_PROMPTS = {
    "pt": ("Escreva apenas as palavras que a pessoa falou; ruído, música e silêncio ficam sem texto.", "Vocabulário"),
    "en": ("Write only the words the speaker said; noise, music and silence get no text.", "Vocabulary"),
}


def is_streaming(model: str) -> bool:
    return model.startswith(STREAMING_MODELS)


def transcribe_prompt(code: str, keywords: list[str]) -> str:
    rule, label = _PROMPTS.get(code, _PROMPTS["en"])
    return f"{rule} {label}: {', '.join(keywords)}." if keywords else rule


@dataclass
class TranscriberCallbacks:
    on_speech_started: Callable[[str], None]
    on_delta: Callable[[str, str], None]
    on_completed: Callable[[str, str], None]
    on_failed: Callable[[str, str], None]
    on_status: Callable[[str, str], None]  # (estado, detalhe)


def session_update(*, model: str, languages: list[str], keywords: list[str], delay: str, silence_ms: int,
                   threshold: float, noise_reduction: str | None) -> dict:
    transcription: dict = {"model": model}
    codes = list(dict.fromkeys(c for c in (lang.split("-")[0].lower() for lang in languages) if c and c != "auto"))
    if model.startswith("gpt-live-transcribe"):
        if codes:
            transcription["languages"] = codes
        transcription["delay"] = delay
        if keywords:
            transcription["keywords"] = keywords
    elif codes:
        transcription["language"] = codes[0]
    if not is_streaming(model):
        transcription["prompt"] = transcribe_prompt(codes[0] if codes else "", keywords)
    audio_input: dict = {
        "format": {"type": "audio/pcm", "rate": RATE},
        "transcription": transcription,
        "turn_detection": None if is_streaming(model) else {
            "type": "server_vad",
            "threshold": threshold,
            "prefix_padding_ms": 300,
            "silence_duration_ms": silence_ms,
        },
    }
    if noise_reduction:
        audio_input["noise_reduction"] = {"type": noise_reduction}
    return {"type": "session.update", "session": {"type": "transcription", "audio": {"input": audio_input}}}


class FatalTranscriberError(RuntimeError):
    """Erro que não adianta tentar de novo (chave inválida, modelo inexistente)."""


class RealtimeSocket:
    """Um WebSocket da Realtime API que recebe áudio por uma fila e reconecta sozinho se cair."""

    APPEND = "input_audio_buffer.append"
    CREATED = ("session.created", "transcription_session.created")
    UPDATED = ("session.updated", "transcription_session.updated")

    def __init__(self, label: str, *, api_key: str, url: str, on_status: Callable[[str, str], None]) -> None:
        self.label = label
        self.api_key = api_key
        self.url = url
        self.on_status = on_status
        self.update: dict = {}
        self.queue: asyncio.Queue[bytes | dict | None] = asyncio.Queue(maxsize=60)  # ~6 s de áudio
        self.seconds_sent = 0.0

    def _put(self, item: bytes | dict | None) -> None:
        if self.queue.full():
            try:
                self.queue.get_nowait()
            except asyncio.QueueEmpty:
                pass
        self.queue.put_nowait(item)

    async def run(self) -> None:
        backoff = 1.0
        while True:
            try:
                await self._session()
                backoff = 1.0
            except asyncio.CancelledError:
                raise
            except FatalTranscriberError as exc:
                log.error("%s: %s", self.label, exc)
                self.on_status("error", str(exc))
                await asyncio.sleep(30)
                continue
            except InvalidStatus as exc:
                status = exc.response.status_code
                detail = exc.response.body.decode(errors="ignore")[:200] if exc.response.body else ""
                log.error("%s: HTTP %s %s", self.label, status, detail)
                if status in (401, 403):
                    self.on_status("error", "Chave da OpenAI recusada (HTTP %s)" % status)
                    await asyncio.sleep(30)
                    continue
                self.on_status("reconnecting", f"HTTP {status}")
            except (OSError, ConnectionClosed, asyncio.TimeoutError) as exc:
                log.warning("%s: conexão caiu: %r", self.label, exc)
                self.on_status("reconnecting", "conexão caiu")
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 20)

    async def _session(self) -> None:
        self.on_status("connecting", "")
        async with connect(
            self.url,
            additional_headers={"Authorization": f"Bearer {self.api_key}"},
            max_size=None,
            open_timeout=15,
            ping_interval=20,
            ping_timeout=20,
        ) as ws:
            await self._handshake(ws)
            self.on_status("listening", "")
            sender = asyncio.create_task(self._send_loop(ws))
            try:
                await self._recv_loop(ws)
            finally:
                sender.cancel()

    async def _handshake(self, ws) -> None:
        async def expect(types: tuple[str, ...]) -> dict:
            while True:
                event = json.loads(await asyncio.wait_for(ws.recv(), 15))
                etype = event.get("type", "")
                if etype == "error":
                    raise FatalTranscriberError(event.get("error", {}).get("message", "erro na sessão"))
                if etype in types:
                    return event

        await expect(self.CREATED)
        await ws.send(json.dumps(self.update))
        await expect(self.UPDATED)
        log.info("%s: sessão pronta", self.label)

    async def _send_loop(self, ws) -> None:
        while True:
            item = await self.queue.get()
            if item is COMMIT:
                await ws.send(json.dumps({"type": "input_audio_buffer.commit"}))
            elif isinstance(item, dict):
                await ws.send(json.dumps(item))
            else:
                await ws.send(json.dumps({"type": self.APPEND, "audio": base64.b64encode(item).decode()}))
                self.seconds_sent += len(item) / (RATE * 2)

    async def _recv_loop(self, ws) -> None:
        raise NotImplementedError


class Transcriber(RealtimeSocket):
    def __init__(
        self,
        label: str,
        *,
        api_key: str,
        base_url: str,
        model: str,
        languages: list[str],
        keywords: list[str],
        delay: str,
        silence_ms: int,
        threshold: float,
        noise_reduction: str | None,
        speech_level: float,
        commit_ms: int,
        silence_gate: bool,
        callbacks: TranscriberCallbacks,
    ) -> None:
        super().__init__(label, api_key=api_key, url=f"{base_url}/realtime?intent=transcription",
                         on_status=callbacks.on_status)
        self.model = model
        self._session_args = dict(model=model, keywords=keywords, delay=delay, silence_ms=silence_ms,
                                  threshold=threshold, noise_reduction=noise_reduction)
        self.update = session_update(languages=languages, **self._session_args)
        self.turns = TurnGate(speech_level=speech_level, commit_ms=commit_ms) if is_streaming(model) else None
        self.gate = SilenceGate() if silence_gate and not self.turns else None
        self.cb = callbacks

    @property
    def speaking(self) -> bool:
        return bool(self.turns and self.turns.open)

    def feed(self, pcm: bytes, level: float) -> None:
        commit = False
        if self.turns:
            chunks, started, commit = self.turns.process(pcm, level)
            if started:
                self.cb.on_speech_started("")
        elif self.gate:
            chunks = self.gate.process(pcm, level)
        else:
            chunks = [pcm]
        for chunk in chunks:
            self._put(chunk)
        if commit:
            self._put(COMMIT)

    def set_languages(self, languages: list[str]) -> None:
        """Troca os idiomas na sessão aberta, sem reconectar (e na próxima conexão)."""
        self.update = session_update(languages=languages, **self._session_args)
        self._put(self.update)

    async def _recv_loop(self, ws) -> None:
        async for raw in ws:
            event = json.loads(raw)
            etype = event.get("type", "")
            if etype == "input_audio_buffer.speech_started":
                self.cb.on_speech_started(event.get("item_id", ""))
            elif etype == "conversation.item.input_audio_transcription.delta":
                self.cb.on_delta(event.get("item_id", ""), event.get("delta", ""))
            elif etype == "conversation.item.input_audio_transcription.completed":
                self.cb.on_completed(event.get("item_id", ""), event.get("transcript", ""))
            elif etype == "conversation.item.input_audio_transcription.failed":
                self.cb.on_failed(event.get("item_id", ""), json.dumps(event.get("error", {}))[:200])
            elif etype == "error":
                err = event.get("error", {})
                if err.get("code") == "input_audio_buffer_commit_empty":
                    continue
                log.warning("%s: erro do servidor: %s", self.label, err)


def translation_update(language: str) -> dict:
    # Sem "transcription": o original já vem do Transcriber; assim não se paga duas vezes.
    return {"type": "session.update", "session": {"audio": {"output": {"language": language.split("-")[0].lower()}}}}


class Translator(RealtimeSocket):
    """Tradução ao vivo (gpt-realtime-translate): o texto traduzido sai ~2 s atrás da fala.

    A API devolve um texto corrido, sem marcar fim de frase nem de fala; quem decide
    onde ele aparece é o motor. O áudio traduzido que ela também manda é ignorado.
    """

    APPEND = "session.input_audio_buffer.append"
    HOLD_CHUNKS = 30  # 3 s de silêncio depois da fala; com menos, ela engole as últimas palavras

    def __init__(self, label: str, *, api_key: str, base_url: str, model: str, language: str,
                 speech_level: float, on_text: Callable[[str], None],
                 on_status: Callable[[str, str], None]) -> None:
        super().__init__(label, api_key=api_key, url=f"{base_url}/realtime/translations?model={model}",
                         on_status=on_status)
        self.model = model
        self.update = translation_update(language)
        self.gate = SilenceGate(floor=speech_level, hold_chunks=self.HOLD_CHUNKS, preroll_chunks=2)
        self.on_text = on_text

    def feed(self, pcm: bytes, level: float) -> None:
        for chunk in self.gate.process(pcm, level):
            self._put(chunk)

    async def _recv_loop(self, ws) -> None:
        async for raw in ws:
            event = json.loads(raw)
            etype = event.get("type", "")
            if etype == "session.output_transcript.delta":
                self.on_text(event.get("delta", ""))
            elif etype == "error":
                log.warning("%s: erro do servidor: %s", self.label, event.get("error", {}))
