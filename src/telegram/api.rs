//! Minimal synchronous Telegram Bot API client built on `ureq`.
//!
//! Only the handful of methods the bridge needs: long-poll `getUpdates`, `sendMessage`
//! (with optional inline keyboard), and `answerCallbackQuery`. Everything is plain text
//! (no MarkdownV2) to avoid escaping pitfalls.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// A single inline-keyboard button: a label and the `callback_data` it carries.
#[derive(Debug, Clone)]
pub struct Button {
    pub text: String,
    pub callback_data: String,
}

pub struct TelegramApi {
    token: String,
    agent: ureq::Agent,
}

impl TelegramApi {
    pub fn new(token: String, poll_timeout_secs: u64) -> Self {
        let agent = ureq::AgentBuilder::new()
            // Read timeout must exceed the long-poll timeout, with headroom.
            .timeout_read(Duration::from_secs(poll_timeout_secs + 15))
            .timeout_write(Duration::from_secs(15))
            .build();
        Self { token, agent }
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    /// Low-level call: POST JSON, return the `result` field, error if `ok != true`.
    fn call(&self, method: &str, body: Value) -> Result<Value> {
        let resp = match self.agent.post(&self.url(method)).send_json(body) {
            Ok(r) => r,
            // Telegram returns 4xx with a JSON error body — read it for a useful message.
            Err(ureq::Error::Status(_code, r)) => r,
            Err(e) => return Err(anyhow!("telegram {method} transport error: {e}")),
        };
        let v: Value = resp
            .into_json()
            .map_err(|e| anyhow!("telegram {method} bad json: {e}"))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let desc = v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("telegram {method} error: {desc}"));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Long-poll for updates. Returns the raw update objects and the next offset to use.
    pub fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>> {
        let body = json!({
            "offset": offset,
            "timeout": timeout_secs,
            "allowed_updates": ["message", "callback_query"],
        });
        let result = self.call("getUpdates", body)?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    /// Send a plain-text message, optionally as a reply and/or with an inline keyboard.
    /// Returns the new message's `message_id`.
    pub fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to_message_id: Option<i64>,
        keyboard: Option<Vec<Vec<Button>>>,
    ) -> Result<i64> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        if let Some(mid) = reply_to_message_id {
            body["reply_to_message_id"] = json!(mid);
        }
        if let Some(rows) = keyboard {
            body["reply_markup"] = json!({ "inline_keyboard": keyboard_json(&rows) });
        }
        let result = self.call("sendMessage", body)?;
        result
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("sendMessage: missing message_id"))
    }

    /// Replace a message's text and remove its inline keyboard (no reply_markup => cleared).
    pub fn edit_message_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        self.call("editMessageText", body)?;
        Ok(())
    }

    /// Delete a previously sent message.
    pub fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let body = json!({ "chat_id": chat_id, "message_id": message_id });
        self.call("deleteMessage", body)?;
        Ok(())
    }

    /// Acknowledge a callback query (stops the spinner on the tapped button).
    pub fn answer_callback_query(&self, callback_query_id: &str, text: Option<&str>) -> Result<()> {
        let mut body = json!({ "callback_query_id": callback_query_id });
        if let Some(t) = text {
            body["text"] = json!(t);
        }
        self.call("answerCallbackQuery", body)?;
        Ok(())
    }
}

fn keyboard_json(rows: &[Vec<Button>]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                Value::Array(
                    row.iter()
                        .map(|b| {
                            json!({
                                "text": b.text,
                                "callback_data": b.callback_data,
                            })
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}
