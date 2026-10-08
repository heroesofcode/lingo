"""Captura de áudio pelo PipeWire (pw-record), um processo por lado da conversa.

- "they": monitor da saída do sistema (o que você ouve na call)
- "me": microfone

Sai PCM16 mono a 24 kHz, que é o formato que a Realtime API da OpenAI espera.
Fechar o processo libera o microfone; isso importa em headset Bluetooth, que
fica preso no perfil de chamada (HFP) enquanto alguém lê o microfone.
"""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Callable

import numpy as np

log = logging.getLogger(__name__)

RATE = 24000
CHUNK_MS = 100
CHUNK_BYTES = RATE * 2 * CHUNK_MS // 1000  # 4800


def rms_level(pcm: bytes) -> float:
    """RMS normalizado em 0..1."""
    if not pcm:
        return 0.0
    samples = np.frombuffer(pcm, dtype="<i2").astype(np.float32)
    return float(np.sqrt(np.mean(samples * samples)) / 32768.0)


def pw_record_command(*, sink_monitor: bool, target: str, label: str) -> list[str]:
    props = [f'media.name="Lingo {label}"', 'application.name="Lingo"', f"node.name=lingo-{label}"]
    if sink_monitor:
        props.append("stream.capture.sink=true")
    cmd = ["pw-record", "--rate", str(RATE), "--channels", "1", "--format", "s16"]
    if target:
        cmd += ["--target", target]
    cmd += ["-P", "{ " + " ".join(props) + " }", "-"]
    return cmd


class Capture:
    """Lê blocos de 100 ms do pw-record e reinicia sozinho se o processo morrer."""

    def __init__(
        self,
        label: str,
        *,
        sink_monitor: bool,
        target: str,
        on_chunk: Callable[[bytes, float], None],
    ) -> None:
        self.label = label
        self.sink_monitor = sink_monitor
        self.target = target
        self.on_chunk = on_chunk
        self._proc: asyncio.subprocess.Process | None = None
        self._stopped = False

    async def run(self) -> None:
        backoff = 0.5
        while not self._stopped:
            cmd = pw_record_command(sink_monitor=self.sink_monitor, target=self.target, label=self.label)
            try:
                self._proc = await asyncio.create_subprocess_exec(
                    *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
                )
            except FileNotFoundError:
                log.error("pw-record não encontrado")
                return
            log.info("captura %s iniciada (pid %s)", self.label, self._proc.pid)
            assert self._proc.stdout is not None
            try:
                while True:
                    pcm = await self._proc.stdout.readexactly(CHUNK_BYTES)
                    self.on_chunk(pcm, rms_level(pcm))
                    backoff = 0.5
            except asyncio.IncompleteReadError:
                pass
            finally:
                await self._terminate()
            if self._stopped:
                break
            err = b""
            if self._proc and self._proc.stderr:
                err = await self._proc.stderr.read()
            log.warning("captura %s terminou (%s); reiniciando", self.label, err.decode(errors="ignore").strip())
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 5)

    async def _terminate(self) -> None:
        proc = self._proc
        if proc and proc.returncode is None:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), 2)
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()

    async def stop(self) -> None:
        self._stopped = True
        await self._terminate()


class TurnGate:
    """Para os modelos ao vivo, que não têm VAD no servidor: manda só os trechos com voz
    (o minuto custa ~6x mais) e decide quando fechar a frase.

    A frase fecha depois de `commit_ms` sem voz, ou de `long_commit_ms` quando já passou de
    `long_ms`, para um monólogo não virar um bloco só. Quando a voz volta, reenvia os últimos
    blocos para não cortar o começo da palavra.
    """

    def __init__(self, *, speech_level: float, commit_ms: int = 700, long_ms: int = 20000,
                 long_commit_ms: int = 300, preroll_chunks: int = 2) -> None:
        self.speech_level = speech_level
        self.commit_ms = commit_ms
        self.long_ms = long_ms
        self.long_commit_ms = long_commit_ms
        self.preroll_chunks = preroll_chunks
        self._open = False
        self._open_ms = 0
        self._quiet_ms = 0
        self._preroll: list[bytes] = []

    @property
    def open(self) -> bool:
        """Há uma frase em andamento (alguém falando ou numa pausa curta)."""
        return self._open

    def process(self, pcm: bytes, level: float) -> tuple[list[bytes], bool, bool]:
        """Devolve (blocos a enviar, começou a falar, fechar a frase)."""
        if level >= self.speech_level:
            started = not self._open
            out = self._preroll + [pcm] if started else [pcm]
            self._preroll = []
            self._open = True
            self._quiet_ms = 0
            self._open_ms += CHUNK_MS
            return out, started, False
        if not self._open:
            self._preroll = (self._preroll + [pcm])[-self.preroll_chunks:]
            return [], False, False
        self._quiet_ms += CHUNK_MS
        self._open_ms += CHUNK_MS
        limit = self.long_commit_ms if self._open_ms >= self.long_ms else self.commit_ms
        if self._quiet_ms < limit:
            return [pcm], False, False
        self._open = False
        self._open_ms = self._quiet_ms = 0
        return [pcm], False, True


class SilenceGate:
    """Para de enviar áudio depois de um tempo em silêncio digital (nada tocando).

    Antes de fechar, deixa passar alguns segundos de silêncio para o VAD do
    servidor fechar a frase. Ao voltar o som, reenvia os últimos blocos.
    """

    def __init__(self, *, floor: float = 1e-4, hold_chunks: int = 25, preroll_chunks: int = 3) -> None:
        self.floor = floor
        self.hold_chunks = hold_chunks
        self.preroll_chunks = preroll_chunks
        self._quiet = 0
        self._preroll: list[bytes] = []

    def process(self, pcm: bytes, level: float) -> list[bytes]:
        if level > self.floor:
            out = self._preroll + [pcm] if self._quiet >= self.hold_chunks else [pcm]
            self._quiet = 0
            self._preroll = []
            return out
        self._quiet += 1
        if self._quiet <= self.hold_chunks:
            return [pcm]
        self._preroll = (self._preroll + [pcm])[-self.preroll_chunks:]
        return []
