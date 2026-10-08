# Lingo

A small always-on-top window for calls in English, made for Portuguese speakers:

- shows what the others say while they speak (each word appears ~1 s after it is said), with the
  Portuguese translation ~2 s behind the speech, and explains expressions such as *circle back* or
  *touch base*;
- when someone asks a question (or says your name), suggests 3 replies in English: direct, complete
  and diplomatic, each with its meaning in Portuguese;
- in the "Como digo…?" ("how do I say…?") field you write in Portuguese and get 3 ways to say it in
  English.

For a call in Portuguese, the `EN→PT` button in the top bar (or `Ctrl+T`) switches to `PT` mode: it
only transcribes, without translation, and the suggested replies come in Portuguese.

The interface is in Brazilian Portuguese for now.

Made for Linux with PipeWire. The window is native GTK4 on Wayland; on Hyprland it floats, stays
pinned on every workspace and has global shortcuts.

## How it works

```
system output ─┬─pw-record──▶ gpt-live-transcribe (English, live) ──▶ window, suggestions
               └────────────▶ gpt-realtime-translate (Portuguese)  ──▶ window
microphone    ──pw-record──▶ gpt-4o-transcribe                    ──▶ context for the suggestions
```

Each side of the conversation is a separate stream, so there is no need to guess who spoke. The
others' English arrives word by word, and the Portuguese comes from a live translator that listens
to the same audio, ~2 s behind the speech, without waiting for the sentence to end. Each utterance
gets its own piece of the translation below it: the translation only moves on to the next utterance
when it pauses itself. The 💡 idiom notes come per sentence, from a separate call to `gpt-5.4-mini`.

Lingo itself closes each utterance after 700 ms without voice. Consecutive utterances from the same
person are grouped in one block, and the utterance in progress has a blue bar on its left. Only the
stretches with voice are sent (the translator also gets 3 s of silence after each utterance: with
less, it swallows the ending).

With `translate = "gpt-5.4-mini"` in `[models]`, translation goes back to sentence by sentence: the
Portuguese of each sentence only appears ~2 s after it ends. It costs half, but on a long sentence
the wait reaches 8–10 s. The live translator does not accept the keyword list, so jargon is
sometimes translated.

Measured with TTS-generated speech, from the end of each word until it appears in the window:

| | median | long sentence (p90) | worst |
|---|---|---|---|
| English | 1.15 s | 1.45 s | — |
| Portuguese, live | 2.2 s | 3.8 s | 4.3 s |
| Portuguese, sentence by sentence (`gpt-5.4-mini`) | 4.4 s | 8.6 s | 10.2 s |

At the end of a question, both ways of translating finish ~2 s after the last word.

The automatic suggestion starts as soon as the "?" shows up, without waiting for the silence: the
first option is complete ~2.3 s after the end of the question, and all three in ~3 s. If the person
keeps talking after the question, it is redone when they stop; meanwhile, the previous options stay
dimmed in the panel.

### Accuracy

What goes wrong most in a developers' call is English jargon in the middle of Portuguese ("o pod"
becomes "pode"). Three things fix it:

- `keywords` in `[languages]`: names, acronyms and project terms, passed to both models;
- in `PT` mode, the others' speech is tagged as Portuguese **and** English;
- `transcribe_delay = "high"`: the model waits for a bit more context before writing.

On 8 developer utterances at ~230 words/min, with noise and Opus at 16 kbps, the word error rate
dropped from 3.5% to 1.1% for the others and from 4.4% to 0.9% for the microphone. Every technical
term came out right, including the ones that were not on the list. The cost is ~0.3 s more per word
(`low` is faster, with three times the errors).

### Asking Claude

For factual questions about your projects ("which version of the payments lib does checkout
use?"), Lingo calls Claude Code (`claude -p`), which has its own memory and reads the code. The
answer appears in the CLAUDE panel, which shows what it is looking at while it searches:

- `SUPER+ALT+A`, `Ctrl+K` or right-clicking an utterance: asks about the others' latest utterance
  (or about that one);
- in the bottom field, `?question` + Enter asks what you typed (in `PT` mode, any text).

If the answer is in its memory, it arrives in ~3–4 s; with one code search, in ~6–12 s; and when it
needs to look through several files, in ~30 s. It runs from `~` (Claude Code's memory is per folder;
change it in `[ask] cwd`), only with read-only tools (Read, Grep, Glob) and without saving the
session to its history. The call content goes to Claude Code, not to OpenAI.

## Install

You need PipeWire (`pw-record`), GTK 4.14 or newer, GLib 2.80 or newer, Rust 1.92 or newer
([rustup](https://rustup.rs)) and an OpenAI API key. To build:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libssl-dev   # Ubuntu 24.04 / Debian
sudo pacman -S --needed base-devel gtk4 openssl                        # Arch / Omarchy
```

Then:

```bash
tools/install.sh   # builds, installs to ~/.local/bin/lingo, creates the .desktop entry and the Hyprland rules
mkdir -p ~/.config/lingo
(umask 077; read -rsp "OpenAI key: " k && echo "$k" > ~/.config/lingo/openai_key)
```

The key can also come from the `OPENAI_API_KEY` variable. If a `SUPER+ALT` shortcut already does
something else in your Hyprland (on Omarchy, for example), `install.sh` comments it out in
`~/.config/hypr/lingo.conf` and tells you.

## Use

| | |
|---|---|
| `SUPER+ALT+L` | open / show / hide |
| `SUPER+ALT+R` | suggest replies to the latest utterance |
| `SUPER+ALT+P` | pause / resume (pausing releases the microphone) |
| `SUPER+ALT+A` or `Ctrl+K` | ask Claude about the latest utterance |
| click an utterance | suggest replies to that utterance |
| right-click an utterance | ask Claude about that utterance |
| `?question` + Enter in the field | ask Claude what you typed |
| click an option or `Ctrl+1..3` | copy the reply |
| `Ctrl+T` or the `EN→PT` / `PT` button | switch between translating and a call in Portuguese |
| `Ctrl+R` `Ctrl+P` `Ctrl+M` `Ctrl+L` `Esc` | suggest, pause, microphone, clear, close panel |

From the command line: `lingo --help`.

Configuration lives in `~/.config/lingo/config.toml` (template in `data/config.example.toml`). The
log is in `~/.local/state/lingo/lingo.log`.

## Costs

The others' speech uses `gpt-live-transcribe` ($0.017 per minute of voice sent; silence is not sent)
and `gpt-realtime-translate` ($0.034 per minute). Your microphone uses `gpt-4o-transcribe`, $0.006
per minute (`mic = "gpt-4o-mini-transcribe"` costs half and makes twice the errors). In a 1-hour
call where the others talk half the time, that is about $1.90; at most $3.40 if they never stop.
With sentence-by-sentence translation (`translate = "gpt-5.4-mini"`), about $0.90. Translation and
suggestions with `gpt-5.4-mini` are a few hundred tokens each. The counter in the top bar shows the
estimated cost of transcription and translation; hover over it to see the minutes sent.

To spend ~6x less on the others' speech, set `transcribe = "gpt-4o-mini-transcribe"` in `[models]`.
The text then only appears when the person stops talking.

## Good to know

- **Bluetooth:** while the microphone is open, the headset stays in the call profile (worse sound).
  Pausing or closing Lingo releases the microphone.
- **Call volume:** only what goes above `speech_level` (0.003) counts as voice. If the "eles"
  (them) meter moves but no text appears, lower this value in `[audio]`; if noise turns into text,
  raise it.
- **Screen sharing:** the window shows up if you share the whole screen. Share only the call window.
- **Privacy:** the call audio goes to OpenAI. Check whether that is allowed by your company and your
  client before using it in meetings with other people.
- **Fonts:** Lingo uses its own fontconfig cache (`~/.cache/lingo`), because Edge writes a cache in
  a newer format to `~/.cache/fontconfig` that breaks apps with an older fontconfig.
- **European Portuguese:** the live translator only accepts `pt`, with no variant, and sometimes
  answers "Implementámos" instead of "Implementamos". There is no way to ask it for pt-BR.

## Code

Rust, with GTK4 ([gtk4-rs](https://gtk-rs.org)) on the main thread and the engine on
[tokio](https://tokio.rs), in its own threads:

| | |
|---|---|
| `src/engine.rs` | the engine: receives everything on a single queue (window commands, OpenAI events) and sends events to the window |
| `src/realtime.rs` | Realtime API WebSockets (live transcription and translation), with reconnection |
| `src/audio.rs` | capture with `pw-record` and the voice/silence gates |
| `src/llm.rs`, `src/prompts.rs`, `src/text.rs` | streaming chat calls, prompts and parsing of the answers |
| `src/ask.rs` | questions to Claude Code |
| `src/ui/` | the window and the command line |

The first version, in Python, is the repository's first commit (`52a9e3e`).

## Tests

```bash
cargo test
```

To open a second instance without touching the one that is open:
`LINGO_APP_ID=dev.pedro.LingoTest LINGO_CONFIG=/path/to/config.toml target/release/lingo`.

## License

MIT. See [LICENSE](LICENSE).
