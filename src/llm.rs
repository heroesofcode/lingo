//! Minimal Chat Completions client with streaming.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};

const REASONING_PREFIXES: [&str; 4] = ["gpt-5", "o1", "o3", "o4"];

#[derive(Clone)]
pub struct Chat {
    http: reqwest::Client,
    url: String,
    api_key: String,
}

impl Chat {
    pub fn new(api_key: &str, base_url: &str) -> Chat {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .read_timeout(Duration::from_secs(30))
            .build()
            .expect("cliente HTTP");
        Chat { http, url: format!("{base_url}/chat/completions"), api_key: api_key.to_string() }
    }

    fn body(model: &str, messages: &Value, temperature: f32, max_tokens: u32, reasoning: bool) -> Value {
        let mut body =
            json!({ "model": model, "messages": messages, "stream": true, "max_completion_tokens": max_tokens });
        if REASONING_PREFIXES.iter().any(|p| model.starts_with(p)) {
            if reasoning {
                body["reasoning_effort"] = json!("none");
            }
        } else {
            body["temperature"] = json!(temperature);
        }
        body
    }

    /// Calls `on_piece` with each piece of the text as it arrives.
    pub async fn stream(
        &self,
        model: &str,
        messages: Value,
        temperature: f32,
        max_tokens: u32,
        mut on_piece: impl FnMut(&str),
    ) -> Result<(), String> {
        let mut reasoning = true;
        for _attempt in 0..2 {
            let body = Self::body(model, &messages, temperature, max_tokens, reasoning);
            let resp = self
                .http
                .post(&self.url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            if !status.is_success() {
                let detail: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
                // a model that rejects reasoning_effort "none": try again without it
                if status == StatusCode::BAD_REQUEST && detail.contains("reasoning_effort") && reasoning {
                    reasoning = false;
                    continue;
                }
                return Err(format!("HTTP {}: {detail}", status.as_u16()));
            }
            let mut bytes = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = bytes.next().await {
                buf.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
                while let Some(end) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&raw);
                    let Some(data) = line.trim().strip_prefix("data:") else { continue };
                    let data = data.trim();
                    if data == "[DONE]" {
                        return Ok(());
                    }
                    let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                    if let Some(piece) = event["choices"][0]["delta"]["content"].as_str()
                        && !piece.is_empty()
                    {
                        on_piece(piece);
                    }
                }
            }
            return Ok(());
        }
        Ok(())
    }
}
