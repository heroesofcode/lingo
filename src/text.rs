//! Transcript cleanup and parsing of the models' (streaming) output.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

/// Phrases the transcription model "makes up" in silence or noise.
const HALLUCINATIONS: [&str; 11] = [
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
];

static NOISE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\[(].*[\])]$").unwrap());
static SENTENCE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[.?!…]+["'”’)\]]*"#).unwrap());
static OPTION_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[*_#>\-\s]*([A-Ca-c])\s*[*_]*\s*[|:)\].]\s*(.*)$").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z']+").unwrap());
static LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(>>>\s*)?((tradução|translation|pt|português|they|eles|me|você)\s*:\s*)?").unwrap()
});

pub fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub fn clean_transcript(text: &str) -> String {
    let text = collapse_spaces(text);
    if !text.chars().any(is_word_char) {
        return String::new();
    }
    let lower = text.to_lowercase();
    let key = lower.trim_matches(|c| " .!?…".contains(c));
    if HALLUCINATIONS.contains(&key)
        || HALLUCINATIONS.contains(&lower.as_str())
        || text.contains('♪')
        || NOISE_TAG.is_match(&text)
    {
        return String::new();
    }
    text
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn space_or_end_after(text: &str, end: usize) -> bool {
    text[end..].chars().next().is_none_or(char::is_whitespace)
}

/// Splits the finished sentences from the text that is still arriving.
///
/// A long sentence without a period is cut at a comma after 14 words; a sentence with fewer than 3
/// ("Pedro.", "Okay.") waits and goes with the next one. Returns the pieces and how many bytes were
/// consumed; with `is_final`, the rest also becomes a piece.
pub fn take_sentences(text: &str, is_final: bool) -> (Vec<String>, usize) {
    const CLAUSE_WORDS: usize = 14;
    const MIN_WORDS: usize = 3;
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut take = |start: &mut usize, end: usize| {
        let piece = collapse_spaces(&text[*start..end]);
        if !piece.is_empty() {
            pieces.push(piece);
        }
        *start = end;
    };

    for m in SENTENCE_END.find_iter(text) {
        if !space_or_end_after(text, m.end()) {
            continue;
        }
        // "3." at the end may still become "3.5"
        let after_digit = text[..m.start()].chars().next_back().is_some_and(char::is_numeric);
        if !is_final && m.end() == text.len() && after_digit {
            break;
        }
        if word_count(&text[start..m.end()]) >= MIN_WORDS {
            take(&mut start, m.end());
        }
    }
    loop {
        let rest = &text[start..];
        let cut = rest.char_indices().find(|&(i, c)| {
            matches!(c, ',' | ';' | ':') && space_or_end_after(rest, i + 1) && word_count(&rest[..i]) >= CLAUSE_WORDS
        });
        let Some((i, _)) = cut else { break };
        let end = start + i + 1;
        take(&mut start, end);
    }
    if is_final {
        take(&mut start, text.len());
    }
    (pieces, start)
}

pub fn is_question(text: &str) -> bool {
    text.contains('?')
}

pub fn mentions(text: &str, names: &[String]) -> bool {
    let lowered = text.to_lowercase();
    names.iter().filter(|name| !name.is_empty()).any(|name| {
        Regex::new(&format!(r"\b{}\b", regex::escape(&name.to_lowercase()))).is_ok_and(|re| re.is_match(&lowered))
    })
}

/// Similar to Python's `SequenceMatcher.ratio()` (Ratcliff/Obershelp): 2·matches / total.
pub fn similar(a: &str, b: &str) -> f64 {
    let norm = |s: &str| -> Vec<char> { s.to_lowercase().chars().filter(|&c| is_word_char(c) || c == ' ').collect() };
    let (a, b) = (norm(a), norm(b));
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    2.0 * matching_chars(&a, &b) as f64 / total as f64
}

fn matching_chars(a: &[char], b: &[char]) -> usize {
    let (mut best, mut at_a, mut at_b) = (0, 0, 0);
    let mut prev = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            if ca == cb {
                cur[j + 1] = prev[j] + 1;
                if cur[j + 1] > best {
                    best = cur[j + 1];
                    at_a = i + 1 - best;
                    at_b = j + 1 - best;
                }
            }
        }
        prev = cur;
    }
    if best == 0 {
        return 0;
    }
    best + matching_chars(&a[..at_a], &b[..at_b]) + matching_chars(&a[at_a + best..], &b[at_b + best..])
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplyOption {
    pub text: String,
    pub gloss: String,
}

/// Reads `A|text` / `a|translation` lines (including the last one, still incomplete).
pub fn parse_options(raw: &str) -> Vec<ReplyOption> {
    let mut options = vec![ReplyOption::default(); 3];
    for line in raw.lines() {
        let Some(m) = OPTION_LINE.captures(line) else { continue };
        let letter = m[1].chars().next().unwrap_or('A');
        let body = m[2].trim().trim_matches('*').trim().to_string();
        let index = (letter.to_ascii_uppercase() as u8 - b'A') as usize;
        let Some(option) = options.get_mut(index) else { continue };
        if letter.is_ascii_uppercase() {
            option.text = body;
        } else {
            option.gloss = body;
        }
    }
    options
}

fn stems(text: &str) -> Vec<String> {
    WORD.find_iter(&text.to_lowercase()).map(|w| w.as_str().chars().take(4).collect()).collect()
}

/// Keeps only the note's expressions that appear in the line itself, without repeats.
pub fn filter_note(note: &str, line: &str) -> String {
    let line_stems: HashSet<String> = stems(line).into_iter().collect();
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    for entry in note.split(';') {
        let Some((expr, _meaning)) = entry.split_once('=') else { continue };
        let expr_stems: Vec<String> = stems(expr).into_iter().filter(|s| s.chars().count() > 2).collect();
        let key = expr_stems.join(" ");
        if !expr_stems.is_empty() && !seen.contains(&key) && expr_stems.iter().all(|s| line_stems.contains(s)) {
            seen.insert(key);
            kept.push(entry.trim());
        }
    }
    kept.join("; ")
}

fn strip_note_prefix(line: &str) -> Option<&str> {
    line.get(..5).filter(|p| p.eq_ignore_ascii_case("NOTE:")).map(|_| line[5..].trim())
}

/// Answer to the notes prompt: `NOTE: ...` or `NONE`.
pub fn parse_note(raw: &str) -> String {
    raw.trim()
        .lines()
        .find_map(|line| strip_note_prefix(line.replace("**", "").trim()).map(str::to_string))
        .unwrap_or_default()
}

/// The first line is the translation; an optional `NOTE:` line explains expressions.
pub fn split_translation(raw: &str) -> (String, String) {
    let lines: Vec<&str> = raw.trim().lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let Some(first) = lines.first() else { return (String::new(), String::new()) };
    let mut translation = LABEL.replace(first, "").trim().trim_matches(['"', '“', '”']).to_string();
    let mut note = String::new();
    for line in &lines[1..] {
        if let Some(rest) = strip_note_prefix(line) {
            note = rest.to_string();
        } else if !note.is_empty() {
            note.push(' ');
            note.push_str(line);
        }
    }
    if strip_note_prefix(&translation).is_some() {
        translation.clear();
    }
    (translation, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(text: &str, is_final: bool) -> Vec<String> {
        take_sentences(text, is_final).0
    }

    #[test]
    fn cuts_finished_sentences_and_keeps_the_rest() {
        let text = " Okay, so quick update on the payment service. We rolled out";
        let (got, used) = take_sentences(text, false);
        assert_eq!(got, ["Okay, so quick update on the payment service."]);
        assert_eq!(&text[used..], " We rolled out");
    }

    #[test]
    fn question_mark_closes_right_away() {
        assert_eq!(
            pieces(" Pedro, you looked into the logs, right?", false),
            ["Pedro, you looked into the logs, right?"]
        );
    }

    #[test]
    fn waits_when_a_number_may_continue() {
        assert_eq!(take_sentences(" It costs 3.", false), (vec![], 0));
        assert_eq!(pieces(" It costs 3.5 dollars.", false), ["It costs 3.5 dollars."]);
    }

    #[test]
    fn long_sentence_is_cut_at_a_comma() {
        let words = vec!["word"; 15].join(" ");
        let text = format!(" {words}, and then more");
        let (got, used) = take_sentences(&text, false);
        assert_eq!(got, [format!("{words},")]);
        assert_eq!(&text[used..], " and then more");
        assert_eq!(take_sentences(" short one, and more", false), (vec![], 0));
    }

    #[test]
    fn tiny_sentence_waits_for_the_next() {
        assert_eq!(take_sentences(" Pedro.", false), (vec![], 0));
        assert_eq!(
            pieces(" Pedro. You looked into the logs, right?", false),
            ["Pedro. You looked into the logs, right?"]
        );
        assert_eq!(pieces(" Right?", true), ["Right?"]);
    }

    #[test]
    fn final_takes_the_rest() {
        assert_eq!(pieces(" It is done. and the tail", true), ["It is done.", "and the tail"]);
    }

    #[test]
    fn drops_known_hallucinations() {
        for text in ["you", "Thanks for watching!", "[Music]", "♪ la la ♪", "...", "  "] {
            assert_eq!(clean_transcript(text), "", "{text}");
        }
    }

    #[test]
    fn keeps_real_speech() {
        assert_eq!(clean_transcript("  Can you   hear me? "), "Can you hear me?");
        assert_eq!(clean_transcript("Thank you, Pedro."), "Thank you, Pedro.");
    }

    #[test]
    fn question_and_name() {
        let names = vec!["Pedro".to_string()];
        assert!(is_question("What do you think?"));
        assert!(!is_question("Let's touch base on Friday."));
        assert!(mentions("Pedro, any update?", &names));
        assert!(!mentions("Pedroso will join later", &names));
    }

    #[test]
    fn echo_similarity() {
        assert!(similar("Can you walk us through it?", "can you walk us through it") > 0.9);
        assert!(similar("Sure, I can do that.", "Can you walk us through it?") < 0.6);
    }

    #[test]
    fn full_options_output() {
        let raw = "A|Sure.\na|Claro.\nB|Yes, because X.\nb|Sim, porque X.\nC|Can I follow up?\nc|Posso retornar?";
        let opts = parse_options(raw);
        let texts: Vec<&str> = opts.iter().map(|o| o.text.as_str()).collect();
        assert_eq!(texts, ["Sure.", "Yes, because X.", "Can I follow up?"]);
        assert_eq!(opts[2].gloss, "Posso retornar?");
    }

    #[test]
    fn partial_stream_and_markdown() {
        let opts = parse_options("**A**| Sure thing\na| Claro\nB| I'd say");
        assert_eq!(opts[0].text, "Sure thing");
        assert_eq!(opts[1].text, "I'd say");
        assert_eq!(opts[2].text, "");
    }

    #[test]
    fn translation_with_note() {
        let (tr, note) = split_translation("Vamos nos falar na sexta.\nNOTE: touch base = falar rapidamente");
        assert_eq!(tr, "Vamos nos falar na sexta.");
        assert_eq!(note, "touch base = falar rapidamente");
    }

    #[test]
    fn strips_leaked_labels() {
        assert_eq!(split_translation(">>> Eles: Honestamente, sim.").0, "Honestamente, sim.");
        assert_eq!(split_translation("Tradução: \"Oi\"").0, "Oi");
    }

    #[test]
    fn note_keeps_only_expressions_from_the_line() {
        let note = "touch base = falar rapidamente; circle back = retomar depois";
        assert_eq!(filter_note(note, "Let's touch base again on Friday."), "touch base = falar rapidamente");
    }

    #[test]
    fn note_dedupes_and_accepts_inflections() {
        let note = "circle back = retomar; circle back = revisit later";
        assert_eq!(filter_note(note, "We're circling back on that."), "circle back = retomar");
    }

    #[test]
    fn notes_only_answer() {
        assert_eq!(parse_note("NOTE: ballpark = estimativa aproximada"), "ballpark = estimativa aproximada");
        assert_eq!(parse_note("**NOTE:** heads-up = aviso"), "heads-up = aviso");
        assert_eq!(parse_note("NONE"), "");
    }

    #[test]
    fn streaming_only_note_prefix() {
        assert_eq!(split_translation("NOTE: x"), (String::new(), String::new()));
    }
}
