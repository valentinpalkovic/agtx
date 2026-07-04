---
name: telegram-bridge-verify
description: Verifies an agtx Telegram bridge change end-to-end by triggering a real notification and inspecting exactly what lands in Telegram, instead of declaring the change done from a code read or `cargo test` pass alone. Use after editing src/telegram/*.rs (extract.rs, daemon.rs, mod.rs, api.rs, commands.rs) - especially message formatting, capture, or the answer/delete flow - or when the user reports a Telegram message looks wrong, is missing context, or goes stale/invisible after answering.
---

# Verify a Telegram bridge change end-to-end

Background: this bridge previously shipped with defects that were only found one at a time through
real usage (missing context in forwarded questions, answers that flash and vanish, raw box-drawing
characters in messages, incomplete message capture). Catch these before reporting the change done.

1. Rebuild the binary so the running daemon uses the change (see the `build-and-symlink` skill).
2. Confirm `[telegram]` in `~/.config/agtx/config.toml` has a real `bot_token` and `allowed_chat_ids`
   entry for a test bot/chat (`GlobalConfig`/`TelegramConfig` in `src/config/mod.rs`).
3. Start the daemon and trigger the actual code path being changed, not a synthetic string:
   ```bash
   agtx telegram-serve
   ```
   - Message-capture/formatting change -> get a task's agent to ask a real question, or let a real
     phase finish, so a genuine notification fires.
   - Answer-capture change -> answer the notification from Telegram itself.
4. Open the real Telegram chat (on the phone client, not just the tmux pane text) and check:
   - The message is the full latest agent turn, captured from the last `⏺` marker onward
     (`extract_marked_message` in `src/telegram/extract.rs`), not a stale or truncated fragment.
   - No raw box-drawing/indentation noise survived (`clean_line` / `is_box_char` in the same file).
   - The message renders correctly in Telegram's Markdown (no broken formatting).
5. Answer from Telegram and confirm the answer surfaces correctly in the terminal, and that the
   original Telegram message is updated/cleared once answered elsewhere (persist-then-delete flow).
6. Only report the change done after this real round-trip - a passing `cargo test --features
   test-mocks` does not confirm real-world formatting or capture correctness for this bridge.
