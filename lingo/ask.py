"""Pergunta ao Claude Code (`claude -p`), que tem a memória do usuário e lê o código dos projetos.

Roda só com ferramentas de leitura e sem salvar a sessão. A resposta chega em
pedaços (texto parcial) e, enquanto ele procura, cada ferramenta usada vira uma
linha de progresso.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
from collections.abc import AsyncIterator
from pathlib import Path

TOOLS = "Read,Grep,Glob"
TIMEOUT_S = 120
SYSTEM = (
    "Esta pergunta chega do Lingo, durante uma call ao vivo. Responda em português, em no máximo "
    "3 frases curtas, direto ao ponto e sem anunciar o que vai fazer. Use só fatos que você "
    "confirmou na memória ou no código dos projetos; se não der para confirmar, diga que não sabe. "
    "Leia o mínimo de arquivos possível. Mantenha nomes de serviços, versões e termos técnicos como estão."
)


class AskError(RuntimeError):
    pass


def find_claude(configured: str) -> str | None:
    if configured:
        return os.path.expanduser(configured)
    # A sessão do Hyprland não tem ~/.local/bin no PATH.
    local = Path.home() / ".local/bin/claude"
    return shutil.which("claude") or (str(local) if local.exists() else None)


def _short(path: str) -> str:
    parts = Path(path).parts
    return "/".join(parts[-2:]) if len(parts) > 2 else path


def describe(tool: dict) -> str:
    name, args = tool.get("name", ""), tool.get("input") or {}
    if name == "Read":
        return f"Lendo {_short(args.get('file_path', ''))}"
    if name == "Grep":
        where = f" em {_short(args['path'])}" if args.get("path") else ""
        return f"Procurando “{args.get('pattern', '')}”{where}"
    if name == "Glob":
        return f"Listando {args.get('pattern', '')}"
    return name


async def ask_claude(command: list[str], cwd: Path, prompt: str) -> AsyncIterator[tuple[str, str]]:
    """Gera ("text", resposta até agora) e ("progress", o que está fazendo)."""
    env = {k: v for k, v in os.environ.items() if k != "XDG_CACHE_HOME"}  # o bin/lingo isola esse cache
    proc = await asyncio.create_subprocess_exec(
        *command, "-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages",
        "--tools", TOOLS, "--no-session-persistence", "--append-system-prompt", SYSTEM,
        cwd=cwd, env=env, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE, limit=32 * 1024 * 1024,  # linhas com o conteúdo de arquivos lidos
    )
    assert proc.stdin and proc.stdout and proc.stderr
    stderr = asyncio.create_task(proc.stderr.read())  # lido em paralelo para o pipe não encher
    proc.stdin.write(prompt.encode())
    await proc.stdin.drain()
    proc.stdin.close()
    text, answered = "", False
    try:
        async with asyncio.timeout(TIMEOUT_S):
            async for line in proc.stdout:
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                kind = ev.get("type")
                if kind == "stream_event":
                    event = ev.get("event") or {}
                    etype, delta = event.get("type"), event.get("delta") or {}
                    if etype == "message_start":
                        text = ""
                    elif etype == "content_block_delta" and delta.get("type") == "text_delta":
                        text += delta.get("text", "")
                        yield "text", text
                    elif etype == "message_delta" and delta.get("stop_reason") == "tool_use" and text:
                        yield "text", ""  # era só um "vou procurar…" antes de usar uma ferramenta
                elif kind == "assistant":
                    for block in (ev.get("message") or {}).get("content", []):
                        if block.get("type") == "tool_use":
                            yield "progress", describe(block)
                elif kind == "result":
                    if ev.get("is_error") or ev.get("subtype") != "success":
                        raise AskError(str(ev.get("result") or ev.get("subtype") or "falhou"))
                    answered = True
                    yield "text", (ev.get("result") or text).strip()
            await proc.wait()
            err = (await stderr).decode(errors="ignore").strip()
    except TimeoutError:
        raise AskError(f"sem resposta em {TIMEOUT_S} s") from None
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
        if not stderr.done():
            stderr.cancel()
    if not answered:
        raise AskError(err[-200:] or f"claude saiu com código {proc.returncode}")
