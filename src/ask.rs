//! Asks Claude Code (`claude -p`), which has the user's memory and reads the projects' code.
//!
//! It runs with read-only tools and without saving the session. The answer arrives in pieces and,
//! while it searches, each tool it uses becomes a progress line.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::{self, expand_home, home};

const TOOLS: &str = "Read,Grep,Glob";
const TIMEOUT: Duration = Duration::from_secs(120);
const SYSTEM: &str = "Esta pergunta chega do Lingo, durante uma call ao vivo. Responda em português, em no máximo \
3 frases curtas, direto ao ponto e sem anunciar o que vai fazer. Use só fatos que você confirmou na memória ou no \
código dos projetos; se não der para confirmar, diga que não sabe. Leia o mínimo de arquivos possível. Mantenha \
nomes de serviços, versões e termos técnicos como estão.";

#[derive(Debug, PartialEq)]
pub enum AskEvent {
    /// the answer so far (empty when the previous text was only a "let me look…")
    Text(String),
    /// what it is looking at
    Progress(String),
}

pub fn find_claude(configured: &str) -> Option<PathBuf> {
    if !configured.is_empty() {
        return Some(expand_home(configured));
    }
    // The Hyprland session may not have ~/.local/bin in its PATH.
    let local = home().join(".local/bin/claude");
    find_in_path("claude").or_else(|| local.exists().then_some(local))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

fn short(path: &str) -> String {
    let parts: Vec<String> = Path::new(path)
        .components()
        .map(|c| match c {
            Component::RootDir => "/".to_string(),
            other => other.as_os_str().to_string_lossy().into_owned(),
        })
        .collect();
    if parts.len() > 2 { parts[parts.len() - 2..].join("/") } else { path.to_string() }
}

pub fn describe(tool: &Value) -> String {
    let name = tool["name"].as_str().unwrap_or_default();
    let arg = |key: &str| tool["input"][key].as_str().unwrap_or_default().to_string();
    match name {
        "Read" => format!("Lendo {}", short(&arg("file_path"))),
        "Grep" => {
            let path = arg("path");
            let place = if path.is_empty() { String::new() } else { format!(" em {}", short(&path)) };
            format!("Procurando “{}”{place}", arg("pattern"))
        }
        "Glob" => format!("Listando {}", arg("pattern")),
        other => other.to_string(),
    }
}

/// Runs `command -p ...` with the prompt on stdin and passes the text and the progress to `on_event`.
pub async fn ask_claude(
    command: &[String],
    cwd: &Path,
    prompt: &str,
    mut on_event: impl FnMut(AskEvent),
) -> Result<(), String> {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .args(["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"])
        .args(["--tools", TOOLS, "--no-session-persistence", "--append-system-prompt", SYSTEM])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match config::cache_home_before_isolation() {
        Some(Some(original)) => {
            cmd.env("XDG_CACHE_HOME", original);
        }
        Some(None) => {
            cmd.env_remove("XDG_CACHE_HOME");
        }
        None => {}
    }
    let mut child = cmd.spawn().map_err(|e| format!("não deu para abrir {}: {e}", command[0]))?;
    let mut stdin = child.stdin.take().expect("stdin com pipe");
    stdin.write_all(prompt.as_bytes()).await.map_err(|e| e.to_string())?;
    drop(stdin);
    let mut stderr = child.stderr.take().expect("stderr com pipe");
    // read in parallel so the pipe does not fill up and stall the process
    let stderr_text = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let mut lines = BufReader::new(child.stdout.take().expect("stdout com pipe")).lines();
    let mut answered = false;
    let read = async {
        let mut text = String::new();
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            let Ok(ev) = serde_json::from_str::<Value>(&line) else { continue };
            match ev["type"].as_str().unwrap_or_default() {
                "stream_event" => {
                    let event = &ev["event"];
                    let delta = &event["delta"];
                    match event["type"].as_str().unwrap_or_default() {
                        "message_start" => text.clear(),
                        "content_block_delta" if delta["type"] == "text_delta" => {
                            text.push_str(delta["text"].as_str().unwrap_or_default());
                            on_event(AskEvent::Text(text.clone()));
                        }
                        "message_delta" if delta["stop_reason"] == "tool_use" && !text.is_empty() => {
                            on_event(AskEvent::Text(String::new()));
                        }
                        _ => {}
                    }
                }
                "assistant" => {
                    for block in ev["message"]["content"].as_array().into_iter().flatten() {
                        if block["type"] == "tool_use" {
                            on_event(AskEvent::Progress(describe(block)));
                        }
                    }
                }
                "result" => {
                    if ev["is_error"] == true || ev["subtype"] != "success" {
                        let why = ev["result"].as_str().or(ev["subtype"].as_str()).unwrap_or("falhou");
                        return Err(why.to_string());
                    }
                    answered = true;
                    let result = ev["result"].as_str().filter(|r| !r.is_empty()).unwrap_or(text.as_str());
                    on_event(AskEvent::Text(result.trim().to_string()));
                }
                _ => {}
            }
        }
        Ok(())
    };
    match tokio::time::timeout(TIMEOUT, read).await {
        Err(_) => return Err(format!("sem resposta em {} s", TIMEOUT.as_secs())),
        Ok(result) => result?,
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let err = stderr_text.await.unwrap_or_default();
    if !answered {
        let err = err.trim();
        let tail: String = err.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(if tail.is_empty() {
            format!("claude saiu com código {}", status.code().unwrap_or(-1))
        } else {
            tail
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "cat > /dev/null\n";

    const ANSWER: &str = r#"
cat <<'EOF'
{"type": "system", "subtype": "init"}
{"type": "stream_event", "event": {"type": "message_start"}}
{"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Vou ver."}}}
{"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "tool_use"}}}
{"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Grep", "input": {"pattern": "payments-lib", "path": "/home/x/code/checkout-service"}}]}}
EOF
printf '{"type": "user", "message": {"content": "'
head -c 300000 /dev/zero | tr '\0' x
printf '"}}\n'
cat <<'EOF'
{"type": "stream_event", "event": {"type": "message_start"}}
{"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Usa a "}}}
{"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "3.2.0."}}}
{"type": "result", "subtype": "success", "is_error": false, "result": "Usa a 3.2.0."}
EOF
"#;

    const FAILS: &str =
        r#"echo '{"type": "result", "subtype": "error_during_execution", "is_error": true, "result": "sem crédito"}'"#;

    const CRASHES: &str = "echo 'not logged in' >&2\nexit 1\n";

    async fn run_fake(script: &str) -> Result<Vec<AskEvent>, String> {
        let dir = std::env::temp_dir().join(format!("lingo-ask-test-{}-{}", std::process::id(), script.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("fake_claude.sh");
        std::fs::write(&fake, format!("{HEADER}{script}")).unwrap();
        let command = vec!["sh".to_string(), fake.to_string_lossy().into_owned()];
        let mut events = Vec::new();
        let result = ask_claude(&command, &dir, "pergunta", |ev| events.push(ev)).await;
        std::fs::remove_dir_all(&dir).ok();
        result.map(|()| events)
    }

    #[tokio::test]
    async fn streams_the_answer_and_what_it_is_looking_at() {
        let events = run_fake(ANSWER).await.unwrap();
        assert_eq!(
            events,
            vec![
                AskEvent::Text("Vou ver.".into()),
                AskEvent::Text(String::new()),
                AskEvent::Progress("Procurando “payments-lib” em code/checkout-service".into()),
                AskEvent::Text("Usa a ".into()),
                AskEvent::Text("Usa a 3.2.0.".into()),
                AskEvent::Text("Usa a 3.2.0.".into()),
            ]
        );
    }

    #[tokio::test]
    async fn error_result_fails() {
        assert_eq!(run_fake(FAILS).await.unwrap_err(), "sem crédito");
    }

    #[tokio::test]
    async fn crash_shows_stderr() {
        assert!(run_fake(CRASHES).await.unwrap_err().contains("not logged in"));
    }
}
