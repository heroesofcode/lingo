"""Limpeza de transcrição e parsers da saída em streaming do modelo."""

from __future__ import annotations

import re
from dataclasses import dataclass
from difflib import SequenceMatcher

# Frases que o modelo de transcrição "inventa" em silêncio ou ruído.
_HALLUCINATIONS = {
    "you",
    "thanks for watching",
    "thank you for watching",
    "thanks for watching!",
    "please subscribe",
    "like and subscribe",
    "subtitles by the amara.org community",
    "[music]",
    "(music)",
    "[blank_audio]",
    "(silence)",
}
_NOISE_TAG = re.compile(r"^[\[(].*[\])]$")


def clean_transcript(text: str) -> str:
    text = " ".join(text.split())
    if not re.search(r"\w", text):
        return ""
    key = text.lower().strip(" .!?…")
    if key in _HALLUCINATIONS or text.lower() in _HALLUCINATIONS or "♪" in text or _NOISE_TAG.match(text):
        return ""
    return text


_SENTENCE_END = re.compile(r"[.?!…]+[\"'”’)\]]*(?=\s|$)")
_CLAUSE_END = re.compile(r"[,;:](?=\s|$)")


def take_sentences(text: str, final: bool, clause_words: int = 14, min_words: int = 3) -> tuple[list[str], int]:
    """Separa as frases já terminadas do texto que ainda está chegando.

    Frase comprida sem ponto é cortada na vírgula depois de `clause_words` palavras; frase com
    menos de `min_words` ("Pedro.", "Okay.") espera e vai junto com a seguinte. Devolve os
    pedaços e quantos caracteres foram consumidos; com `final`, o resto também vira pedaço.
    """
    pieces: list[str] = []
    start = 0

    def take(end: int) -> None:
        nonlocal start
        piece = " ".join(text[start:end].split())
        if piece:
            pieces.append(piece)
        start = end

    for match in _SENTENCE_END.finditer(text):
        # "3." no fim ainda pode virar "3.5"
        if not final and match.end() == len(text) and text[match.start() - 1:match.start()].isdigit():
            break
        if len(text[start:match.end()].split()) >= min_words:
            take(match.end())
    while True:
        rest = text[start:]
        cut = next((m for m in _CLAUSE_END.finditer(rest) if len(rest[:m.start()].split()) >= clause_words), None)
        if cut is None:
            break
        take(start + cut.end())
    if final:
        take(len(text))
    return pieces, start


def is_question(text: str) -> bool:
    return "?" in text


def mentions(text: str, names: list[str]) -> bool:
    lowered = text.lower()
    return any(re.search(rf"\b{re.escape(n.lower())}\b", lowered) for n in names if n)


def similar(a: str, b: str) -> float:
    norm = lambda s: re.sub(r"[^\w ]", "", s.lower())
    return SequenceMatcher(None, norm(a), norm(b)).ratio()


@dataclass
class Option:
    text: str = ""
    gloss: str = ""


_OPTION_LINE = re.compile(r"^\s*[*_#>\-\s]*([A-Ca-c])\s*[*_]*\s*[|:)\].]\s*(.*)$")


def parse_options(raw: str, count: int = 3) -> list[Option]:
    """Lê linhas `A|texto` / `a|tradução` (também a última, ainda incompleta)."""
    options = [Option() for _ in range(count)]
    for line in raw.splitlines():
        match = _OPTION_LINE.match(line)
        if not match:
            continue
        letter, body = match.group(1), match.group(2).strip().strip("*").strip()
        idx = ord(letter.upper()) - ord("A")
        if not 0 <= idx < count:
            continue
        if letter.isupper():
            options[idx].text = body
        else:
            options[idx].gloss = body
    return options


def _stems(text: str) -> list[str]:
    return [w[:4] for w in re.findall(r"[a-z']+", text.lower())]


def filter_note(note: str, line: str) -> str:
    """Mantém só as expressões da nota que aparecem na própria frase, sem repetir."""
    line_stems = set(_stems(line))
    kept, seen = [], set()
    for entry in note.split(";"):
        expr, sep, _meaning = entry.partition("=")
        stems = [s for s in _stems(expr) if len(s) > 2] if sep else []
        key = " ".join(stems)
        if stems and key not in seen and all(s in line_stems for s in stems):
            seen.add(key)
            kept.append(entry.strip())
    return "; ".join(kept)


def parse_note(raw: str) -> str:
    """Resposta do prompt de notas: `NOTE: ...` ou `NONE`."""
    for line in raw.strip().splitlines():
        line = line.replace("**", "").strip()
        if line.upper().startswith("NOTE:"):
            return line[5:].strip()
    return ""


_LABEL = re.compile(
    r"^\s*(>>>\s*)?((tradução|translation|pt|português|they|eles|me|você)\s*:\s*)?", re.IGNORECASE
)


def split_translation(raw: str) -> tuple[str, str]:
    """Primeira linha é a tradução; uma linha `NOTE:` opcional explica expressões."""
    lines = [ln.strip() for ln in raw.strip().splitlines() if ln.strip()]
    if not lines:
        return "", ""
    translation = _LABEL.sub("", lines[0]).strip().strip('"“”')
    note = ""
    for ln in lines[1:]:
        if ln.upper().startswith("NOTE:"):
            note = ln[5:].strip()
        elif note:
            note += " " + ln
    if translation.upper().startswith("NOTE:"):
        translation = ""
    return translation, note
