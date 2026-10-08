"""Orquestra captura, transcrição, tradução e sugestões.

Roda num loop asyncio próprio (thread separada da interface). Tudo o que a
interface precisa saber chega por `emit(evento)`, um dict com "type".
"""

from __future__ import annotations

import asyncio
import logging
import threading
import time
import uuid
from collections.abc import Callable, Coroutine
from dataclasses import dataclass, field
from datetime import datetime
from functools import partial
from pathlib import Path
from typing import Any

from . import prompts
from .ask import ask_claude, find_claude
from .audio import Capture
from .config import DATA_DIR, MODES, Config, price_per_min
from .llm import Chat
from .realtime import Transcriber, TranscriberCallbacks, Translator
from .text import (
    clean_transcript,
    filter_note,
    is_question,
    mentions,
    parse_note,
    parse_options,
    similar,
    split_translation,
    take_sentences,
)

log = logging.getLogger(__name__)

THEY, ME = "THEY", "ME"
MAX_UTTERANCES = 400
ECHO_WINDOW_S = 20
AUTO_MIN_INTERVAL_S = 3.0
BLOCK_GAP_S = 20  # falas seguidas da mesma pessoa com menos que isso entre elas formam um bloco
TR_SETTLE_S = 0.8  # a tradução ao vivo só passa para a fala seguinte depois de uma pausa dela


@dataclass
class Segment:
    """Pedaço de uma fala (uma frase, em geral) traduzido sozinho, assim que termina."""

    source: str
    translation: str = ""
    note: str = ""
    done: bool = False


@dataclass
class Utterance:
    uid: str
    speaker: str
    started: float
    text: str = ""
    final: bool = False
    live: str = ""  # deltas como chegaram; os cortes para tradução são feitos sobre ele
    cut: int = 0  # quanto de `live` já foi mandado traduzir
    segments: list[Segment] = field(default_factory=list)
    translation: str = ""
    note: str = ""
    saved_translation: bool = False
    asked_words: int = 0  # palavras que a fala tinha quando a sugestão saiu no "?"
    block: str = ""  # uid da primeira fala do bloco (falas seguidas da mesma pessoa)
    updated: float = 0.0  # time.monotonic() do último texto


@dataclass
class Side:
    capture: Capture
    transcriber: Transcriber
    tasks: list[asyncio.Task]
    translator: Translator | None = None
    translator_task: asyncio.Task | None = None

    def sockets(self) -> list[Transcriber | Translator]:
        return [self.transcriber] + ([self.translator] if self.translator else [])


class Engine:
    def __init__(self, cfg: Config, api_key: str, emit: Callable[[dict], None]) -> None:
        self.cfg = cfg
        self.api_key = api_key
        self.emit = emit
        self.chat = Chat(api_key, cfg.api_base)
        self.utts: dict[str, Utterance] = {}
        self.mode = cfg.mode
        self.paused = False
        self.mic_enabled = cfg.capture_mic
        self._sides: dict[str, Side] = {}
        self._levels = {THEY: 0.0, ME: 0.0}
        self._spent_min = 0.0
        self._spent_usd = 0.0
        self._meter: asyncio.Task | None = None
        self._suggest_task: asyncio.Task | None = None
        self._ask_task: asyncio.Task | None = None
        self._translate_sem = asyncio.Semaphore(3)
        self._background: set[asyncio.Task] = set()
        self._auto_target: Utterance | None = None
        self._auto_handle: asyncio.TimerHandle | None = None
        self._last_auto = 0.0
        # tradução ao vivo: o texto corrido é repartido entre as falas de "eles"
        self._tr_uid: str | None = None
        self._tr_pending = ""
        self._tr_last = 0.0
        self._transcript_file = None
        if cfg.save_transcripts:
            path = DATA_DIR / "sessions" / f"{datetime.now():%Y-%m-%d_%H%M}.md"
            path.parent.mkdir(parents=True, exist_ok=True)
            self._transcript_file = path

    @property
    def translating(self) -> bool:
        return self.mode == "translate"

    @property
    def live_translation(self) -> bool:
        return self.translating and self.cfg.live_translation

    # ---- ciclo de vida -------------------------------------------------

    async def start(self) -> None:
        self._meter = asyncio.create_task(self._meter_loop())
        await self._start_listening()

    async def shutdown(self) -> None:
        await self._stop_listening()
        if self._meter:
            self._meter.cancel()
        if self._ask_task and not self._ask_task.done():
            self._ask_task.cancel()  # mata o `claude` que estiver rodando
            await asyncio.gather(self._ask_task, return_exceptions=True)
        await self.chat.close()

    async def set_paused(self, paused: bool) -> None:
        if paused == self.paused:
            return
        self.paused = paused
        if paused:
            self._cancel_auto()
            await self._stop_listening()
        else:
            await self._start_listening()
        self.emit({"type": "paused", "value": paused})

    async def set_mic(self, enabled: bool) -> None:
        self.mic_enabled = enabled
        if not self.paused:
            if enabled:
                self._start_side(ME)
            else:
                await self._stop_side(ME)
        self.emit({"type": "mic", "enabled": enabled})

    async def set_mode(self, mode: str) -> None:
        if mode not in MODES or mode == self.mode:
            return
        self.mode = mode
        self._cancel_auto()
        they, me = self.cfg.languages(mode)
        for speaker, side in self._sides.items():
            side.transcriber.set_languages(they if speaker == THEY else me)
        if THEY in self._sides:
            if self.live_translation:
                self._start_translator(self._sides[THEY])
            else:
                self._stop_translator(self._sides[THEY])
        self.emit({"type": "mode", "mode": mode})

    async def clear(self) -> None:
        self.utts.clear()
        self._cancel_auto()
        self._tr_uid, self._tr_pending = None, ""
        self.emit({"type": "cleared"})

    async def _start_listening(self) -> None:
        self._start_side(THEY)
        if self.mic_enabled:
            self._start_side(ME)

    async def _stop_listening(self) -> None:
        for speaker in list(self._sides):
            await self._stop_side(speaker)

    def _start_side(self, speaker: str) -> None:
        if speaker in self._sides:
            return
        cfg = self.cfg
        label = "eles" if speaker == THEY else "voce"
        they_langs, me_langs = cfg.languages(self.mode)
        callbacks = TranscriberCallbacks(
            on_speech_started=partial(self._on_speech_started, speaker),
            on_delta=partial(self._on_delta, speaker),
            on_completed=partial(self._on_completed, speaker),
            on_failed=partial(self._on_failed, speaker),
            on_status=lambda state, detail, s=speaker: self.emit(
                {"type": "status", "side": s, "state": state, "detail": detail}),
        )
        transcriber = Transcriber(
            label,
            api_key=self.api_key,
            base_url=cfg.realtime_base,
            model=cfg.transcribe_model if speaker == THEY else cfg.mic_model,
            languages=they_langs if speaker == THEY else me_langs,
            keywords=cfg.vocabulary(),
            delay=cfg.transcribe_delay,
            silence_ms=cfg.vad_silence_ms,
            threshold=cfg.vad_threshold,
            noise_reduction="near_field" if speaker == ME else None,
            speech_level=cfg.speech_level,
            commit_ms=cfg.commit_ms,
            silence_gate=speaker == THEY,
            callbacks=callbacks,
        )

        def on_chunk(pcm: bytes, level: float) -> None:
            self._levels[speaker] = max(self._levels[speaker], level)
            transcriber.feed(pcm, level)
            if side.translator:
                side.translator.feed(pcm, level)

        capture = Capture(label, sink_monitor=speaker == THEY,
                          target=cfg.they_target if speaker == THEY else cfg.mic_target, on_chunk=on_chunk)
        side = Side(capture, transcriber, [asyncio.create_task(capture.run()), asyncio.create_task(transcriber.run())])
        self._sides[speaker] = side
        if speaker == THEY and self.live_translation:
            self._start_translator(side)

    def _start_translator(self, side: Side) -> None:
        if side.translator:
            return
        side.translator = Translator(
            "traducao", api_key=self.api_key, base_url=self.cfg.realtime_base, model=self.cfg.translate_model,
            language=self.cfg.translate_to, speech_level=self.cfg.speech_level, on_text=self._on_live_translation,
            on_status=lambda state, detail: self.emit({"type": "status", "side": "TR", "state": state,
                                                       "detail": detail}))
        side.translator_task = asyncio.create_task(side.translator.run())

    def _stop_translator(self, side: Side) -> None:
        if not side.translator:
            return
        if side.translator_task:
            side.translator_task.cancel()
        self._spend(side.translator)
        side.translator = side.translator_task = None
        self.emit({"type": "status", "side": "TR", "state": "stopped", "detail": ""})

    def _spend(self, socket: Transcriber | Translator) -> None:
        self._spent_min += socket.seconds_sent / 60
        self._spent_usd += socket.seconds_sent / 60 * price_per_min(socket.model)

    async def _stop_side(self, speaker: str) -> None:
        side = self._sides.pop(speaker, None)
        if not side:
            return
        await side.capture.stop()
        for task in side.tasks:
            task.cancel()
        self._stop_translator(side)
        self._spend(side.transcriber)
        self.emit({"type": "status", "side": speaker, "state": "stopped", "detail": ""})

    async def _meter_loop(self) -> None:
        tick = 0
        while True:
            await asyncio.sleep(0.1)
            self.emit({"type": "levels", THEY: self._levels[THEY], ME: self._levels[ME]})
            self._levels = {THEY: 0.0, ME: 0.0}
            tick += 1
            if tick % 20 == 0:
                live = [sock for side in self._sides.values() for sock in side.sockets()]
                minutes = self._spent_min + sum(t.seconds_sent for t in live) / 60
                usd = self._spent_usd + sum(t.seconds_sent / 60 * price_per_min(t.model) for t in live)
                self.emit({"type": "usage", "minutes": minutes, "usd": usd})

    def _spawn(self, coro: Coroutine[Any, Any, None]) -> None:
        task = asyncio.create_task(coro)
        self._background.add(task)  # sem referência o loop pode descartar a tarefa
        task.add_done_callback(self._background.discard)

    # ---- eventos da transcrição ---------------------------------------

    def _utterance(self, speaker: str, item_id: str) -> Utterance:
        uid = f"{speaker}:{item_id}"
        utt = self.utts.get(uid)
        if utt is None:
            prev = next(reversed(self.utts.values()), None)
            cont = prev is not None and prev.speaker == speaker and time.monotonic() - prev.updated < BLOCK_GAP_S
            utt = Utterance(uid, speaker, time.time(), block=prev.block if cont else uid, updated=time.monotonic())
            self.utts[uid] = utt
            while len(self.utts) > MAX_UTTERANCES:
                self.utts.pop(next(iter(self.utts)))
            self.emit({"type": "utt_start", "uid": uid, "speaker": speaker, "cont": cont})
        return utt

    def _on_speech_started(self, speaker: str, _item_id: str) -> None:
        # A linha só aparece com o primeiro texto, para não piscar com ruído.
        if speaker == ME:
            self._cancel_auto()
        elif self._auto_handle:
            # Ainda estão falando: espera terminarem antes de sugerir.
            self._auto_handle.cancel()
            self._auto_handle = None

    def _on_delta(self, speaker: str, item_id: str, delta: str) -> None:
        utt = self._utterance(speaker, item_id)
        if utt.final:
            return
        utt.live += delta
        utt.text = " ".join(utt.live.split())
        utt.updated = time.monotonic()
        self.emit({"type": "utt_partial", "uid": utt.uid, "text": utt.text})
        if speaker != THEY:
            return
        if self.translating:
            self._cut(utt, final=False)
        if self.cfg.auto_suggest and utt.text.endswith("?") and len(utt.text.split()) > utt.asked_words:
            # Sugere já no "?", sem esperar o silêncio que fecha a fala (~1,2 s a menos).
            utt.asked_words = len(utt.text.split())
            self._cancel_auto()
            self._last_auto = time.monotonic()
            self._start_suggest(self._suggest(utt, "auto"))

    def _on_failed(self, speaker: str, item_id: str, error: str) -> None:
        log.warning("transcrição falhou (%s): %s", speaker, error)
        self._drop(f"{speaker}:{item_id}")

    def _drop(self, uid: str) -> None:
        if self.utts.pop(uid, None) is not None:
            self.emit({"type": "utt_remove", "uid": uid})
        if self._auto_target and not self._auto_handle:
            self._arm_auto(self._auto_target)

    def _on_completed(self, speaker: str, item_id: str, transcript: str) -> None:
        utt = self._utterance(speaker, item_id)
        text = clean_transcript(transcript)
        if text and speaker == ME and self._is_echo(text):
            log.info("descartado eco do microfone: %r", text)
            text = ""
        if not text:
            self._drop(utt.uid)
            return
        utt.text, utt.final, utt.updated = text, True, time.monotonic()
        self.emit({"type": "utt_final", "uid": utt.uid, "speaker": speaker, "text": text})
        self._save(utt)
        if speaker != THEY:
            return
        if self.translating:
            if not utt.live:
                utt.live = text
            self._cut(utt, final=True)
            self._save_translation(utt)
        if utt.asked_words and len(text.split()) <= utt.asked_words:
            return  # já sugerido no "?" e nada foi dito depois
        if self.cfg.auto_suggest and (is_question(text) or mentions(text, self.cfg.my_names)):
            self._arm_auto(utt)
        elif self._auto_target:
            self._arm_auto(self._auto_target)

    def _is_echo(self, text: str) -> bool:
        now = time.time()
        return any(
            u.speaker == THEY and u.final and now - u.started < ECHO_WINDOW_S and similar(text, u.text) > 0.75
            for u in list(self.utts.values())[-12:]
        )

    # ---- gatilho automático -------------------------------------------

    def _arm_auto(self, utt: Utterance) -> None:
        self._auto_target = utt
        if self._auto_handle:
            self._auto_handle.cancel()
        delay = max(self.cfg.suggest_delay_s, AUTO_MIN_INTERVAL_S - (time.monotonic() - self._last_auto))
        self._auto_handle = asyncio.get_running_loop().call_later(delay, self._fire_auto)

    def _fire_auto(self) -> None:
        self._auto_handle = None
        side = self._sides.get(THEY)
        if side and side.transcriber.speaking:
            return  # emendaram outra frase; quando ela terminar o gatilho é armado de novo
        target, self._auto_target = self._auto_target, None
        if target and target.uid in self.utts and not self.paused:
            self._last_auto = time.monotonic()
            self._start_suggest(self._suggest(target, "auto"))

    def _cancel_auto(self) -> None:
        if self._auto_handle:
            self._auto_handle.cancel()
        self._auto_handle = None
        self._auto_target = None

    # ---- tradução -----------------------------------------------------

    def _context(self, upto: Utterance | None, n: int) -> tuple[list[tuple[str, str]], int | None]:
        items = [u for u in self.utts.values() if u.final or (u is upto and u.text)]
        if upto is not None and upto in items:
            end = items.index(upto) + 1
            window = items[max(0, end - n):end]
            return [(u.speaker, u.text) for u in window], len(window) - 1
        window = items[-n:]
        return [(u.speaker, u.text) for u in window], None

    def _cut(self, utt: Utterance, final: bool) -> None:
        """Manda traduzir cada frase que terminou, sem esperar a pessoa parar de falar."""
        pieces, used = take_sentences(utt.live[utt.cut:], final)
        utt.cut += used
        for piece in pieces:
            seg = Segment(piece)
            utt.segments.append(seg)
            self._spawn(self._translate(utt, seg))

    def _translation_context(self, utt: Utterance, seg: Segment) -> tuple[list[tuple[str, str]], int]:
        before = []
        for u in self.utts.values():
            if u is utt:
                break
            if u.final:
                before.append((u.speaker, u.text))
        lines = before[-3:]
        i = utt.segments.index(seg)
        prior = " ".join(s.source for s in utt.segments[max(0, i - 2):i])
        if prior:
            lines.append((utt.speaker, prior))
        lines.append((utt.speaker, seg.source))
        return lines, len(lines) - 1

    async def _translate(self, utt: Utterance, seg: Segment) -> None:
        """Traduz o pedaço; com a tradução ao vivo ligada, só explica as expressões dele."""
        live = self.cfg.live_translation
        async with self._translate_sem:
            if utt.uid not in self.utts:
                return
            lines, idx = self._translation_context(utt, seg)
            target = self.cfg.language_name(self.cfg.translate_to)
            messages = [
                {"role": "system", "content": prompts.notes_system(target) if live else prompts.translate_system(target)},
                {"role": "user", "content": prompts.transcript_block(lines, idx)},
            ]
            model = self.cfg.notes_model if live else self.cfg.translate_model
            raw = ""
            try:
                async for piece in self.chat.stream(model, messages, temperature=0, max_tokens=220):
                    raw += piece
                    if not live:
                        seg.translation, note = split_translation(raw)
                        seg.note = filter_note(note, seg.source)
                        self._emit_translation(utt)
            except Exception as exc:  # noqa: BLE001 - mostrar qualquer falha na interface
                log.warning("tradução/notas falhou: %s", exc)
                if not live:  # sem as notas dá para seguir; sem a tradução, não
                    self.emit({"type": "error", "text": f"Tradução falhou: {exc}"})
            if live:
                seg.note = filter_note(parse_note(raw), seg.source)
            else:
                seg.translation, note = split_translation(raw)
                seg.note = filter_note(note, seg.source)
            seg.done = True
            self._emit_translation(utt)
            self._save_translation(utt)

    def _emit_translation(self, utt: Utterance) -> None:
        if not self.cfg.live_translation:
            utt.translation = " ".join(s.translation for s in utt.segments if s.translation)
            self.emit({"type": "translation", "uid": utt.uid, "text": utt.translation})
        utt.note = filter_note("; ".join(s.note for s in utt.segments if s.note), utt.live or utt.text)
        self.emit({"type": "note", "uid": utt.uid, "note": utt.note})

    # ---- tradução ao vivo (gpt-realtime-translate) ---------------------

    def _on_live_translation(self, delta: str) -> None:
        """Texto corrido da tradução; vai para a fala de "eles" que está sendo traduzida.

        A API não marca fim de frase nem de fala, e o texto vem ~2 s atrás do original. A tradução
        só passa para a fala seguinte depois de uma pausa dela, então nunca adianta: no pior caso,
        o fim de uma frase aparece embaixo da fala seguinte.
        """
        now = time.monotonic()
        newest = next((u for u in reversed(self.utts.values()) if u.speaker == THEY), None)
        current = self.utts.get(self._tr_uid) if self._tr_uid else None
        if newest and newest is not current and (current is None or now - self._tr_last >= TR_SETTLE_S):
            if current:
                self._save_live_translation(current)
            current, self._tr_uid = newest, newest.uid
            current.translation, self._tr_pending = self._tr_pending + current.translation, ""
        self._tr_last = now
        if current is None:  # traduzido antes de a fala aparecer na tela
            self._tr_pending += delta
            return
        current.translation += delta
        self.emit({"type": "translation", "uid": current.uid, "text": " ".join(current.translation.split())})

    # ---- sugestões ----------------------------------------------------

    async def suggest(self, uid: str | None = None) -> None:
        target = self.utts.get(uid) if uid else None
        if target is None:
            target = next((u for u in reversed(self.utts.values()) if u.speaker == THEY and u.text), None)
        if target is None:
            self.emit({"type": "notice", "text": "Ainda não há fala dos outros para responder."})
            return
        self._cancel_auto()
        self._start_suggest(self._suggest(target, "manual"))

    async def ask(self, question: str = "", uid: str | None = None) -> None:
        """Pergunta ao Claude Code: o texto digitado ou, sem ele, a fala dos outros (a última ou `uid`)."""
        question = question.strip()
        target = None
        if not question:
            target = self.utts.get(uid) if uid else None
            target = target or next((u for u in reversed(self.utts.values()) if u.speaker == THEY and u.text), None)
            if target is None:
                self.emit({"type": "notice", "text": "Ainda não há fala dos outros para perguntar ao Claude."})
                return
        command = find_claude(self.cfg.ask_command)
        if not command:
            self.emit({"type": "error", "text": "Claude Code não encontrado; configure [ask] command."})
            return
        if self._ask_task and not self._ask_task.done():
            self._ask_task.cancel()
        self._ask_task = asyncio.create_task(self._ask(command, question, target))

    async def _ask(self, command: str, question: str, target: Utterance | None) -> None:
        req = uuid.uuid4().hex[:8]
        lines, idx = self._context(target, 8)
        prompt = prompts.ask_prompt(question, prompts.transcript_block(lines, idx))
        self.emit({"type": "ask_start", "req": req, "question": question or target.text})
        try:
            async for kind, text in ask_claude([command], Path(self.cfg.ask_cwd).expanduser(), prompt):
                self.emit({"type": f"ask_{kind}", "req": req, "text": text})
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 - mostrar qualquer falha na interface
            log.warning("pergunta ao Claude falhou: %s", exc)
            self.emit({"type": "error", "text": f"Claude: {exc}"})
        self.emit({"type": "ask_done", "req": req})

    async def phrase(self, draft: str) -> None:
        draft = draft.strip()
        if draft:
            self._start_suggest(self._phrase(draft))

    def _start_suggest(self, coro: Coroutine[Any, Any, None]) -> None:
        if self._suggest_task and not self._suggest_task.done():
            self._suggest_task.cancel()
        self._suggest_task = asyncio.create_task(coro)

    async def _suggest(self, target: Utterance, reason: str) -> None:
        lines, idx = self._context(target, self.cfg.context_lines)
        native = self.cfg.language_name(self.cfg.translate_to)
        if self.translating:
            system = prompts.suggest_system(self.cfg.language_name(self.cfg.reply_language), native)
        else:
            system = prompts.suggest_system(native, None)
        user = "Call transcript:\n" + prompts.transcript_block(lines, idx)
        # Com a tradução ao vivo, no "?" o português da pergunta ainda está chegando: título em inglês.
        subtitle = target.translation if self.translating and not self.live_translation else ""
        await self._stream_options("reply", target.text, subtitle, reason, system, user, 0.7)

    async def _phrase(self, draft: str) -> None:
        lines, _ = self._context(None, 8)
        system = prompts.phrase_system(self.cfg.language_name(self.cfg.reply_language),
                                       self.cfg.language_name(self.cfg.translate_to))
        user = "Call transcript (context):\n" + prompts.transcript_block(lines, None) + f"\n\nDraft: {draft}"
        await self._stream_options("phrase", draft, "", "manual", system, user, 0.6)

    async def _stream_options(self, mode: str, title: str, subtitle: str, reason: str,
                              system: str, user: str, temperature: float) -> None:
        req = uuid.uuid4().hex[:8]
        self.emit({"type": "sugg_start", "req": req, "mode": mode, "title": title,
                   "subtitle": subtitle, "reason": reason})
        raw = ""
        try:
            async for piece in self.chat.stream(self.cfg.suggest_model,
                                                [{"role": "system", "content": system},
                                                 {"role": "user", "content": user}],
                                                temperature=temperature, max_tokens=600):
                raw += piece
                self.emit({"type": "sugg_options", "req": req,
                           "options": [(o.text, o.gloss) for o in parse_options(raw)]})
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001
            log.warning("sugestão falhou: %s", exc)
            self.emit({"type": "error", "text": f"Sugestão falhou: {exc}"})
        self.emit({"type": "sugg_done", "req": req})

    # ---- histórico opcional -------------------------------------------

    def _save(self, utt: Utterance) -> None:
        if not self._transcript_file:
            return
        stamp = datetime.fromtimestamp(utt.started).strftime("%H:%M:%S")
        who = "Eles" if utt.speaker == THEY else "Você"
        with self._transcript_file.open("a") as fh:
            fh.write(f"- `{stamp}` **{who}:** {utt.text}\n")

    def _save_translation(self, utt: Utterance) -> None:
        if (not self._transcript_file or utt.saved_translation or not utt.final or not utt.segments
                or not all(s.done for s in utt.segments)):
            return
        utt.saved_translation = True
        with self._transcript_file.open("a") as fh:
            # a tradução ao vivo já é gravada por _save_live_translation
            fh.write((f"  - _{utt.translation}_\n" if utt.translation and not self.cfg.live_translation else "")
                     + (f"  - 💡 {utt.note}\n" if utt.note else ""))

    def _save_live_translation(self, utt: Utterance) -> None:
        text = " ".join(utt.translation.split())
        if self._transcript_file and text:
            with self._transcript_file.open("a") as fh:
                fh.write(f"  - _{text}_\n")


class EngineThread:
    """Loop asyncio numa thread; a interface chama `submit(coro)`."""

    def __init__(self) -> None:
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(target=self._run, name="lingo-engine", daemon=True)
        self.thread.start()

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.run_forever()

    def submit(self, coro: Coroutine[Any, Any, Any]):
        return asyncio.run_coroutine_threadsafe(coro, self.loop)

    def stop(self, timeout: float = 4) -> None:
        self.loop.call_soon_threadsafe(self.loop.stop)
        self.thread.join(timeout)
