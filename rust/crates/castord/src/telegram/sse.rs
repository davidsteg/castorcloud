//! The assistant's (Hermes) SSE chat stream, reduced to the outcome the bot
//! needs (packages/core/src/assistant.ts): the answer, or why there is none.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(String),
    /// The tenant's monthly AI budget is used up.
    Credit,
    Failed,
}

/// LiteLLM's answer once the tenant's budget is used up.
fn budget(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("budget has been exceeded") || t.contains("budget_exceeded")
}

/// One SSE event, as `translate` (deltas and tool events don't end a turn).
pub fn translate(event: &str, data: &Value) -> Option<Outcome> {
    match event {
        "assistant.completed" => {
            let text = data.get("content").and_then(Value::as_str).unwrap_or("");
            // Hermes reports provider errors as the turn's text.
            Some(if budget(text) {
                Outcome::Credit
            } else {
                Outcome::Done(text.to_owned())
            })
        }
        "run.failed" => Some(if budget(&data.to_string()) {
            Outcome::Credit
        } else {
            Outcome::Failed
        }),
        _ => None,
    }
}

/// Fed the body chunk by chunk; yields the outcome once one arrives.
#[derive(Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub fn feed(&mut self, chunk: &[u8]) -> Option<Outcome> {
        self.buffer.extend_from_slice(chunk);
        while let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..end + 2).collect();
            let block = String::from_utf8_lossy(block.get(..end).unwrap_or_default());
            let mut event = "message";
            let mut data = String::new();
            for line in block.split('\n') {
                if let Some(e) = line.strip_prefix("event:") {
                    event = e.trim();
                } else if let Some(d) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d.trim_start());
                }
            }
            let parsed = if data.is_empty() {
                Value::Object(Default::default())
            } else {
                match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                }
            };
            if let Some(out) = translate(event, &parsed) {
                return Some(out);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream() {
        let mut p = Parser::default();
        assert_eq!(
            p.feed(b"event: assistant.delta\ndata: {\"delta\":\"Hi\"}\n\nevent: tool.st"),
            None
        );
        assert_eq!(p.feed(b"arted\ndata: {}\n\nevent: assistant.compl"), None);
        assert_eq!(
            p.feed(b"eted\ndata: {\"content\":\"Hi there\"}\n\n"),
            Some(Outcome::Done("Hi there".into()))
        );
        let mut p = Parser::default();
        assert_eq!(
            p.feed(b"event: run.failed\ndata: {\"error\":\"Budget has been exceeded!\"}\n\n"),
            Some(Outcome::Credit)
        );
        let mut p = Parser::default();
        assert_eq!(
            p.feed(b"event: run.failed\ndata: not json\n\nevent: run.failed\ndata: {}\n\n"),
            Some(Outcome::Failed)
        );
        assert_eq!(
            translate(
                "assistant.completed",
                &serde_json::json!({ "content": "litellm budget_exceeded" })
            ),
            Some(Outcome::Credit)
        );
    }
}
