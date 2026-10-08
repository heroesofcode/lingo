"""Prompts de tradução, sugestão de resposta e "como digo isso?"."""

from __future__ import annotations


def translate_system(target: str) -> str:
    return (
        f"You translate live work-call speech into {target}.\n"
        "Translate ONLY the line marked >>> into natural, everyday "
        f"{target}. Earlier lines are context only.\n"
        "Line 1 of your answer: the translation, nothing else (no quotes, no labels).\n"
        "Then, if the line contains an idiom, phrasal verb, slang, business jargon or a "
        "false friend that a Brazilian professional might not know (e.g. circle back, touch base, "
        "ballpark, heads-up, on my plate, ping me, push back, ASAP, actually, eventually), add one line:\n"
        f"NOTE: <expression> = <short meaning in {target}>; <expression> = <meaning>\n"
        "The NOTE covers only expressions that appear in the >>> line itself, never ones from "
        "the context lines, and never plain vocabulary or technical nouns (e.g. deployment plan, "
        "timeout). Keep names, numbers and product terms as they are."
    )


def notes_system(target: str) -> str:
    return (
        "You help a Brazilian professional follow a live work call in English.\n"
        "Look ONLY at the line marked >>> (earlier lines are context). If it contains an idiom, phrasal "
        "verb, slang, business jargon or a false friend they might not know (e.g. circle back, touch base, "
        "ballpark, heads-up, on my plate, ping me, push back, boil the ocean, actually, eventually), answer "
        f"with one line:\nNOTE: <expression> = <short meaning in {target}>; <expression> = <meaning>\n"
        "Otherwise answer exactly: NONE\n"
        "Never explain plain vocabulary, technical nouns or acronyms (e.g. deployment, timeout, sprint, PRD)."
    )


def suggest_system(reply_lang: str, gloss_lang: str | None) -> str:
    """Sem `gloss_lang` a call é no idioma nativo: nada de tradução nem de inglês simplificado."""
    if gloss_lang:
        who = "ME, a Brazilian software professional whose English is intermediate,"
        style = "spoken, natural, simple words that are easy to pronounce"
        output = (f"After each option add its meaning in {gloss_lang}.\n"
                  "Output format, exactly these 6 lines and nothing else:\n"
                  "A|<option A>\na|<meaning of A>\nB|<option B>\nb|<meaning of B>\nC|<option C>\nc|<meaning of C>")
    else:
        who = "ME, a Brazilian software professional,"
        style = "spoken and natural, the way a colleague would say it"
        output = ("Output format, exactly these 3 lines and nothing else:\n"
                  "A|<option A>\nB|<option B>\nC|<option C>")
    return (
        f"You help {who} reply out loud in a live work call. THEY are the other participants.\n"
        f"Write exactly 3 reply options in {reply_lang} that ME could say next, "
        "answering the line marked >>>:\n"
        "A = short and direct\n"
        "B = more complete; adds a reason, an example or a clarifying question\n"
        "C = diplomatic: buys time, asks to follow up, or softens a disagreement\n"
        f"Rules: {style}; max 30 words each; "
        "first person; A, B and C must each begin with a different first word and differ in "
        "intent, not just in length.\n"
        "You do NOT know what ME found, decided, did or thinks beyond what ME said in the transcript. "
        "Never guess causes, results, numbers, dates or commitments: wherever the answer depends on "
        "them, write a short bracketed placeholder for ME to fill in, e.g. [cause], [finding], [date].\n"
        + output
    )


def phrase_system(reply_lang: str, gloss_lang: str) -> str:
    return (
        "ME is in a live work call and wants to say something but is not sure how. "
        f"ME wrote a draft in {gloss_lang} or in broken {reply_lang}.\n"
        f"Write exactly 3 ways to say it out loud in natural {reply_lang}, fitting the call "
        "transcript given as context:\n"
        "A = simple and direct\nB = polished and professional\nC = friendly and casual\n"
        "Keep the meaning of the draft; do not add facts. Max 35 words each.\n"
        f"After each option add a short note in {gloss_lang} on the nuance or a tricky word.\n"
        "Output format, exactly these 6 lines and nothing else:\n"
        "A|<option A>\na|<note>\nB|<option B>\nb|<note>\nC|<option C>\nc|<note>"
    )


def ask_prompt(question: str, transcript: str) -> str:
    """Pergunta para o Claude Code: a digitada ou, sem ela, a fala marcada com >>>."""
    head = (f"Pergunta: {question}" if question else
            "Na call acabaram de me perguntar a fala marcada com >>>. Diga o que eu preciso saber para responder.")
    return f"{head}\n\nÚltimas falas da call (THEY = os outros, ME = eu):\n{transcript}"


def transcript_block(lines: list[tuple[str, str]], target_index: int | None) -> str:
    out = []
    for i, (speaker, text) in enumerate(lines):
        mark = ">>> " if i == target_index else ""
        out.append(f"{mark}{speaker}: {text}")
    return "\n".join(out) if out else "(no transcript yet)"
