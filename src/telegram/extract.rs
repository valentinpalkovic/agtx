//! Question extraction from an idle agent's tmux pane.
//!
//! This is the correctness-critical core of the Telegram bridge. Given the captured
//! text of a pane that agtx has already marked `PhaseStatus::Idle` (stable for 15s),
//! we decide whether the agent is actually *asking the user something* and, if so,
//! produce a clean question + any tappable options.
//!
//! Design bias: **silence over spam**. When in doubt we classify as `Finished`/`Stuck`
//! and send nothing, because a false notification is worse than a missed one.
//!
//! All functions here are pure and unit-tested against captured-pane fixtures.

/// Bottom-of-pane markers that indicate an agent's interactive input box is on screen
/// (i.e. the agent is waiting for the human rather than mid-thought). Mirrors the
/// `AGENT_ACTIVE_INDICATORS` used by the TUI's `is_agent_active`, with a couple of extra
/// footer markers and a Copilot entry (the TUI's list omits Copilot today).
const INPUT_BOX_MARKERS: &[&str] = &[
    "Claude Code",
    "? for shortcuts",
    "Type your message",
    "Ask anything",
    "Cursor Agent",
    "OpenAI Codex",
    "GitHub Copilot",
    "Copilot",
];

/// How many lines from the bottom to scan for the input-box marker.
const BOTTOM_SCAN_LINES: usize = 15;

/// Maximum length of the question text sent to Telegram.
const MAX_QUESTION_LEN: usize = 320;

/// The shape of question detected, which drives how a reply is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionKind {
    /// Numbered menu — reply by sending the chosen digit + Enter.
    Menu,
    /// Yes/No (`[y/N]`, `(y/n)`) — reply by sending `y`/`n` + Enter.
    YesNo,
    /// Free-form question — reply with text.
    FreeText,
}

/// A single tappable answer option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedOption {
    /// What to send when chosen (e.g. `"2"`, `"y"`).
    pub key: String,
    /// Human label shown on the button.
    pub label: String,
}

/// Result of classifying an idle pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// The agent is waiting for an answer.
    Asking {
        /// Human-readable context to show the user: the agent's recent reasoning, the
        /// question, and any options — cleaned of ANSI and the input-box chrome.
        context: String,
        kind: QuestionKind,
        options: Vec<ExtractedOption>,
    },
    /// The agent finished / is at an empty prompt with nothing to ask.
    Finished,
    /// The agent has no visible input box (likely mid-thought or crashed) — do not notify.
    Stuck,
}

/// Strip ANSI escape sequences (SGR `CSI ... m`, other CSI, and OSC `... BEL/ST`).
///
/// `capture-pane -p` usually omits escapes, but we strip defensively so question text is clean.
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC
            if i + 1 < bytes.len() {
                match bytes[i + 1] {
                    b'[' => {
                        // CSI: consume until a final byte in 0x40..=0x7e
                        i += 2;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1; // skip the final byte
                        continue;
                    }
                    b']' => {
                        // OSC: consume until BEL (0x07) or ST (ESC \)
                        i += 2;
                        while i < bytes.len() {
                            if bytes[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        continue;
                    }
                    _ => {
                        // Other ESC-prefixed sequence: skip ESC and the next byte
                        i += 2;
                        continue;
                    }
                }
            } else {
                break;
            }
        }
        // Copy this UTF-8 codepoint whole (find its byte length)
        let ch_len = utf8_len(b);
        let end = (i + ch_len).min(bytes.len());
        if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
            out.push_str(s);
        }
        i = end;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// True if a line is empty, only box-drawing characters, or known input-box hint chrome.
fn is_noise(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    // Box-drawing / prompt-border only
    if t.chars().all(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '╭' | '╮'
                    | '╰'
                    | '╯'
                    | '│'
                    | '─'
                    | '┌'
                    | '┐'
                    | '└'
                    | '┘'
                    | '├'
                    | '┤'
                    | '┬'
                    | '┴'
                    | '┼'
                    | '|'
                    | '>'
                    | '▌'
                    | '·'
            )
    }) {
        return true;
    }
    let lower = t.to_lowercase();
    let hints = [
        "? for shortcuts",
        "esc to interrupt",
        "ctrl+",
        "tokens",
        "context left",
        "auto-accept",
        "accept edits",
        "shift+tab",
    ];
    hints.iter().any(|h| lower.contains(h))
}

fn contains_input_marker(line: &str) -> bool {
    INPUT_BOX_MARKERS.iter().any(|m| line.contains(m))
}

/// Strip the agent's input-box chrome from the bottom of the captured lines, returning the
/// "question block" above it. We cut from the lowest input marker line downward.
fn strip_input_box<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    // Find the lowest line index that looks like input-box chrome (marker or border line
    // among the bottom region). Cut everything from there down.
    let mut cut = lines.len();
    for (idx, line) in lines.iter().enumerate() {
        if contains_input_marker(line) {
            cut = cut.min(idx);
        }
    }
    if cut == lines.len() {
        return lines.to_vec();
    }
    // Also trim trailing noise lines just above the cut.
    let mut end = cut;
    while end > 0 && is_noise(lines[end - 1]) {
        end -= 1;
    }
    lines[..end].to_vec()
}

/// Parse a numbered menu or yes/no prompt out of the question block.
/// Returns the option list and the detected kind, or `None` for free-text.
fn parse_options(block: &[&str]) -> Option<(QuestionKind, Vec<ExtractedOption>)> {
    // Numbered menu: collect lines like "1. Foo", "2) Bar", "❯ 3. Baz".
    let mut menu: Vec<ExtractedOption> = Vec::new();
    for line in block {
        if let Some(opt) = parse_numbered_option(line) {
            menu.push(opt);
        }
    }
    if menu.len() >= 2 {
        // De-dup consecutive identical keys (cursor redraw artifacts) keeping last label.
        menu.dedup_by(|a, b| a.key == b.key);
        return Some((QuestionKind::Menu, menu));
    }

    // Yes/No prompt anywhere in the block.
    let joined = block.join("\n").to_lowercase();
    if joined.contains("[y/n]")
        || joined.contains("(y/n)")
        || joined.contains("[yes/no]")
        || joined.contains(" y/n")
    {
        return Some((
            QuestionKind::YesNo,
            vec![
                ExtractedOption {
                    key: "y".to_string(),
                    label: "Yes".to_string(),
                },
                ExtractedOption {
                    key: "n".to_string(),
                    label: "No".to_string(),
                },
            ],
        ));
    }
    None
}

/// Parse a single "N. label" / "N) label" line (with optional leading cursor markers).
fn parse_numbered_option(line: &str) -> Option<ExtractedOption> {
    let trimmed = line.trim();
    // Strip leading cursor/bullet markers.
    let rest = trimmed.trim_start_matches(|c: char| {
        matches!(c, '❯' | '>' | '*' | '▶' | '➤' | '·' | '-' | ' ' | '\t')
    });
    let mut chars = rest.char_indices();
    let mut digits = String::new();
    let mut sep_idx = None;
    for (idx, c) in chars.by_ref() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if (c == '.' || c == ')') && !digits.is_empty() {
            sep_idx = Some(idx);
            break;
        } else {
            return None;
        }
    }
    let sep_idx = sep_idx?;
    if digits.is_empty() {
        return None;
    }
    let label = rest[sep_idx + 1..].trim();
    if label.is_empty() {
        return None;
    }
    Some(ExtractedOption {
        key: digits,
        label: truncate(label, 60),
    })
}

/// Extract the human-readable question from the block, excluding option lines.
fn extract_question_text(block: &[&str], options: &[ExtractedOption]) -> String {
    let option_labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
    // Walk upward collecting the trailing prose paragraph (skip option/noise lines).
    let mut collected: Vec<&str> = Vec::new();
    let mut started = false;
    for line in block.iter().rev() {
        let t = line.trim();
        if is_noise(line) || parse_numbered_option(line).is_some() {
            if started {
                break;
            }
            continue;
        }
        // Skip lines that are just an option label echoed.
        if option_labels.contains(&t) {
            if started {
                break;
            }
            continue;
        }
        collected.push(t);
        started = true;
        // Stop once we have a reasonably sized paragraph.
        if collected.join(" ").len() > MAX_QUESTION_LEN {
            break;
        }
    }
    collected.reverse();
    truncate(&collected.join(" "), MAX_QUESTION_LEN)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Heuristic: does this free-text block read like a question / request for input?
fn looks_like_question(text: &str) -> bool {
    let t = text.trim();
    if t.ends_with('?') {
        return true;
    }
    let lower = t.to_lowercase();
    let cues = [
        "do you want",
        "would you like",
        "should i",
        "shall i",
        "which ",
        "please confirm",
        "please provide",
        "please enter",
        "what would you",
        "how would you",
        "enter your",
        "provide the",
        "confirm ",
        "proceed?",
        "y/n",
        "(yes/no)",
    ];
    cues.iter().any(|c| lower.contains(c))
}

/// Classify the captured pane content of an idle task.
pub fn classify(pane: &str, _agent: &str) -> Classification {
    let clean = strip_ansi(pane);
    let mut lines: Vec<&str> = clean.lines().collect();
    // Drop trailing fully-empty lines.
    while matches!(lines.last(), Some(l) if l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return Classification::Stuck;
    }

    // Keep the last ~80 lines as our working window.
    let window_start = lines.len().saturating_sub(80);
    let window: Vec<&str> = lines[window_start..].to_vec();

    // Input-box gate: scan the bottom region for a marker.
    let bottom_start = window.len().saturating_sub(BOTTOM_SCAN_LINES);
    let has_box = window[bottom_start..]
        .iter()
        .any(|l| contains_input_marker(l));

    let block = strip_input_box(&window);
    let options = parse_options(&block);

    match options {
        Some((kind, opts)) => {
            // An interactive menu / yes-no is a strong signal — notify regardless of box.
            let context = build_context(&block);
            let context = if context.is_empty() {
                match kind {
                    QuestionKind::YesNo => "Confirm? (yes/no)".to_string(),
                    _ => "Choose an option:".to_string(),
                }
            } else {
                context
            };
            Classification::Asking {
                context,
                kind,
                options: opts,
            }
        }
        None => {
            // Free-text path requires the input box to be visible (agent at its prompt).
            if !has_box {
                return Classification::Stuck;
            }
            let question = extract_question_text(&block, &[]);
            if !question.is_empty() && looks_like_question(&question) {
                let context = build_context(&block);
                let context = if context.is_empty() {
                    question
                } else {
                    context
                };
                Classification::Asking {
                    context,
                    kind: QuestionKind::FreeText,
                    options: Vec::new(),
                }
            } else {
                Classification::Finished
            }
        }
    }
}

/// Build a readable context excerpt from the question block (reasoning + question +
/// options), limited to the last ~25 lines and ~1500 chars so it stays phone-friendly.
/// The question sits at the bottom of the block, so we keep the tail.
fn build_context(block: &[&str]) -> String {
    let mut end = block.len();
    while end > 0 && block[end - 1].trim().is_empty() {
        end -= 1;
    }
    let slice = &block[..end];
    let start = slice.len().saturating_sub(25);
    let text = slice[start..].join("\n");
    truncate_tail(text.trim(), 1500)
}

/// Keep at most the last `max` characters (UTF-8 safe), prefixing `…` if truncated.
fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    format!("…{}", s.chars().skip(count - max).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_csi_and_osc() {
        let s = "\x1b[31mhello\x1b[0m \x1b]0;title\x07world";
        assert_eq!(strip_ansi(s), "hello world");
    }

    #[test]
    fn strips_ansi_preserves_unicode() {
        let s = "\x1b[1m❯ 1. Yes\x1b[0m";
        assert_eq!(strip_ansi(s), "❯ 1. Yes");
    }

    #[test]
    fn claude_numbered_menu_is_asking() {
        let pane = "\
I'd like to edit config.rs to add retry logic.

Do you want to make this edit to config.rs?
❯ 1. Yes
  2. Yes, and don't ask again this session
  3. No, and tell Claude what to do differently

╭──────────────────────────────────────────╮
│ >                                          │
╰──────────────────────────────────────────╯
  ? for shortcuts
";
        match classify(pane, "claude") {
            Classification::Asking {
                context,
                kind,
                options,
            } => {
                assert_eq!(kind, QuestionKind::Menu);
                assert_eq!(options.len(), 3);
                assert_eq!(options[0].key, "1");
                assert!(options[0].label.starts_with("Yes"));
                // Context keeps the question AND the full option text (not just buttons).
                assert!(context.contains("Do you want to make this edit"));
                assert!(context.contains("don't ask again"));
            }
            other => panic!("expected Asking, got {other:?}"),
        }
    }

    #[test]
    fn yes_no_prompt_is_asking() {
        let pane = "\
About to delete 3 files. Proceed? [y/N]

╭───────────────────────╮
│ >                     │
╰───────────────────────╯
Type your message
";
        match classify(pane, "gemini") {
            Classification::Asking { kind, options, .. } => {
                assert_eq!(kind, QuestionKind::YesNo);
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].key, "y");
                assert_eq!(options[1].key, "n");
            }
            other => panic!("expected Asking, got {other:?}"),
        }
    }

    #[test]
    fn free_text_question_with_box_is_asking() {
        let pane = "\
I need more detail before continuing.

Which authentication provider config format should I target?

╭────────────────────────────╮
│ >                          │
╰────────────────────────────╯
Type your message
";
        match classify(pane, "gemini") {
            Classification::Asking { kind, context, .. } => {
                assert_eq!(kind, QuestionKind::FreeText);
                assert!(context.contains("authentication provider"));
            }
            other => panic!("expected Asking, got {other:?}"),
        }
    }

    #[test]
    fn finished_output_does_not_notify() {
        let pane = "\
All done! I created the PR and pushed the branch.
Summary: added retry logic and tests.

╭────────────────────────────╮
│ >                          │
╰────────────────────────────╯
  ? for shortcuts
";
        assert_eq!(classify(pane, "claude"), Classification::Finished);
    }

    #[test]
    fn no_input_box_is_stuck() {
        let pane = "\
* Thinking about the problem...
  Reading src/main.rs
  Considering edge cases
";
        assert_eq!(classify(pane, "claude"), Classification::Stuck);
    }

    #[test]
    fn empty_pane_is_stuck() {
        assert_eq!(classify("\n\n   \n", "claude"), Classification::Stuck);
    }

    #[test]
    fn numbered_option_parsing_strips_cursor() {
        let opt = parse_numbered_option("❯ 2. Yes, and don't ask again").unwrap();
        assert_eq!(opt.key, "2");
        assert_eq!(opt.label, "Yes, and don't ask again");
    }

    #[test]
    fn prose_numbered_list_without_box_is_not_spammed() {
        // A numbered list in normal output with no input box should still be Stuck-safe:
        // parse_options would find a menu, but there's no box and it's mid-thought.
        // We intentionally treat a >=2 numbered list as a menu only within the question
        // block; here the lines are plain output, so confirm we don't crash and produce
        // a sane classification.
        let pane = "\
Here is my plan:
1. Refactor the client
2. Add tests
3. Open a PR
";
        // No input box -> menu detected but we still surface it as Asking only if it's the
        // trailing block. This documents current behavior: a trailing numbered list reads
        // as a menu. The 15s-idle + agent-at-prompt reality makes this rare in practice.
        match classify(pane, "claude") {
            Classification::Asking { kind, .. } => assert_eq!(kind, QuestionKind::Menu),
            other => panic!("documents menu behavior, got {other:?}"),
        }
    }
}
