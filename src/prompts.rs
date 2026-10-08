//! Prompts for translation, reply suggestions and "how do I say this?".

use crate::engine::Speaker;

pub fn translate_system(target: &str) -> String {
    format!(
        "You translate live work-call speech into {target}.\n\
         Translate ONLY the line marked >>> into natural, everyday {target}. Earlier lines are context only.\n\
         Line 1 of your answer: the translation, nothing else (no quotes, no labels).\n\
         Then, if the line contains an idiom, phrasal verb, slang, business jargon or a false friend that a \
         Brazilian professional might not know (e.g. circle back, touch base, ballpark, heads-up, on my plate, \
         ping me, push back, ASAP, actually, eventually), add one line:\n\
         NOTE: <expression> = <short meaning in {target}>; <expression> = <meaning>\n\
         The NOTE covers only expressions that appear in the >>> line itself, never ones from the context lines, \
         and never plain vocabulary or technical nouns (e.g. deployment plan, timeout). Keep names, numbers and \
         product terms as they are."
    )
}

pub fn notes_system(target: &str) -> String {
    format!(
        "You help a Brazilian professional follow a live work call in English.\n\
         Look ONLY at the line marked >>> (earlier lines are context). If it contains an idiom, phrasal verb, \
         slang, business jargon or a false friend they might not know (e.g. circle back, touch base, ballpark, \
         heads-up, on my plate, ping me, push back, boil the ocean, actually, eventually), answer with one line:\n\
         NOTE: <expression> = <short meaning in {target}>; <expression> = <meaning>\n\
         Otherwise answer exactly: NONE\n\
         Never explain plain vocabulary, technical nouns or acronyms (e.g. deployment, timeout, sprint, PRD)."
    )
}

/// Without `gloss_lang` the call is in the native language: no translation and no simplified English.
pub fn suggest_system(reply_lang: &str, gloss_lang: Option<&str>) -> String {
    let (who, style, output) = match gloss_lang {
        Some(gloss) => (
            "ME, a Brazilian software professional whose English is intermediate,",
            "spoken, natural, simple words that are easy to pronounce",
            format!(
                "After each option add its meaning in {gloss}.\n\
                 Output format, exactly these 6 lines and nothing else:\n\
                 A|<option A>\na|<meaning of A>\nB|<option B>\nb|<meaning of B>\nC|<option C>\nc|<meaning of C>"
            ),
        ),
        None => (
            "ME, a Brazilian software professional,",
            "spoken and natural, the way a colleague would say it",
            "Output format, exactly these 3 lines and nothing else:\nA|<option A>\nB|<option B>\nC|<option C>"
                .to_string(),
        ),
    };
    format!(
        "You help {who} reply out loud in a live work call. THEY are the other participants.\n\
         Write exactly 3 reply options in {reply_lang} that ME could say next, answering the line marked >>>:\n\
         A = short and direct\n\
         B = more complete; adds a reason, an example or a clarifying question\n\
         C = diplomatic: buys time, asks to follow up, or softens a disagreement\n\
         Rules: {style}; max 30 words each; first person; A, B and C must each begin with a different first \
         word and differ in intent, not just in length.\n\
         You do NOT know what ME found, decided, did or thinks beyond what ME said in the transcript. Never \
         guess causes, results, numbers, dates or commitments: wherever the answer depends on them, write a \
         short bracketed placeholder for ME to fill in, e.g. [cause], [finding], [date].\n\
         {output}"
    )
}

pub fn phrase_system(reply_lang: &str, gloss_lang: &str) -> String {
    format!(
        "ME is in a live work call and wants to say something but is not sure how. ME wrote a draft in \
         {gloss_lang} or in broken {reply_lang}.\n\
         Write exactly 3 ways to say it out loud in natural {reply_lang}, fitting the call transcript given \
         as context:\n\
         A = simple and direct\nB = polished and professional\nC = friendly and casual\n\
         Keep the meaning of the draft; do not add facts. Max 35 words each.\n\
         After each option add a short note in {gloss_lang} on the nuance or a tricky word.\n\
         Output format, exactly these 6 lines and nothing else:\n\
         A|<option A>\na|<note>\nB|<option B>\nb|<note>\nC|<option C>\nc|<note>"
    )
}

/// Question for Claude Code: the typed one or, without it, the line marked with >>>.
pub fn ask_prompt(question: &str, transcript: &str) -> String {
    let head = if question.is_empty() {
        "Na call acabaram de me perguntar a fala marcada com >>>. Diga o que eu preciso saber para responder."
            .to_string()
    } else {
        format!("Pergunta: {question}")
    };
    format!("{head}\n\nÚltimas falas da call (THEY = os outros, ME = eu):\n{transcript}")
}

pub fn transcript_block(lines: &[(Speaker, String)], target: Option<usize>) -> String {
    if lines.is_empty() {
        return "(no transcript yet)".to_string();
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, (speaker, text))| {
            let mark = if Some(i) == target { ">>> " } else { "" };
            format!("{mark}{}: {text}", speaker.label())
        })
        .collect::<Vec<_>>()
        .join("\n")
}
