"""Cliente mínimo de Chat Completions com streaming (aiohttp)."""

from __future__ import annotations

import json
import logging
from collections.abc import AsyncIterator

import aiohttp

log = logging.getLogger(__name__)

_REASONING_PREFIXES = ("gpt-5", "o1", "o3", "o4")


class LLMError(RuntimeError):
    pass


class Chat:
    def __init__(self, api_key: str, base_url: str) -> None:
        self.api_key = api_key
        self.url = f"{base_url}/chat/completions"
        self._session: aiohttp.ClientSession | None = None

    async def _http(self) -> aiohttp.ClientSession:
        if self._session is None or self._session.closed:
            self._session = aiohttp.ClientSession(
                timeout=aiohttp.ClientTimeout(total=60, sock_read=30),
                headers={"Authorization": f"Bearer {self.api_key}"},
            )
        return self._session

    async def close(self) -> None:
        if self._session and not self._session.closed:
            await self._session.close()

    @staticmethod
    def _body(model: str, messages: list[dict], temperature: float, max_tokens: int,
              reasoning: bool) -> dict:
        body = {"model": model, "messages": messages, "stream": True, "max_completion_tokens": max_tokens}
        if model.startswith(_REASONING_PREFIXES):
            if reasoning:
                body["reasoning_effort"] = "none"
        else:
            body["temperature"] = temperature
        return body

    async def stream(self, model: str, messages: list[dict], *, temperature: float = 0.3,
                     max_tokens: int = 400) -> AsyncIterator[str]:
        http = await self._http()
        reasoning = True
        for _attempt in range(2):
            body = self._body(model, messages, temperature, max_tokens, reasoning)
            async with http.post(self.url, json=body) as resp:
                if resp.status != 200:
                    detail = (await resp.text())[:300]
                    if resp.status == 400 and "reasoning_effort" in detail and reasoning:
                        reasoning = False
                        continue
                    raise LLMError(f"HTTP {resp.status}: {detail}")
                async for raw in resp.content:
                    line = raw.decode(errors="ignore").strip()
                    if not line.startswith("data:"):
                        continue
                    data = line[5:].strip()
                    if data == "[DONE]":
                        return
                    chunk = json.loads(data)
                    choices = chunk.get("choices") or []
                    if choices:
                        piece = (choices[0].get("delta") or {}).get("content")
                        if piece:
                            yield piece
                return
