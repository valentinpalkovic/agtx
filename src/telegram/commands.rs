//! Telegram command parsing and board rendering (pure, unit-tested).

use crate::db::{Task, TaskStatus};

/// A parsed bot command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Show the board.
    Board,
    /// Advance a task to its next phase (by short id).
    Advance(String),
    /// Resume a Review task back to Running (by short id).
    Resume(String),
    /// Create a new backlog task with the given title.
    New(String),
    /// Answer a specific task with free text: `/answer <id> <text>`.
    Answer { id: String, text: String },
    /// Set the active task for bare replies.
    Select(String),
    /// View the orchestrator's conversation (empty arg) or send it a message.
    Orchestrator(String),
    /// Show help.
    Help,
    /// Unrecognized command.
    Unknown(String),
}

/// Parse a `/command ...` message into a [`Command`].
pub fn parse_command(text: &str) -> Command {
    let text = text.trim();
    let mut parts = text.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    // Strip leading '/' and any "@botname" suffix.
    let cmd = head.trim_start_matches('/');
    let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();

    match cmd.as_str() {
        "board" | "b" | "tasks" => Command::Board,
        "advance" | "next" | "a" => Command::Advance(first_token(rest).to_string()),
        "resume" => Command::Resume(first_token(rest).to_string()),
        "new" | "create" => Command::New(rest.to_string()),
        "select" | "sel" => Command::Select(first_token(rest).to_string()),
        "orch" | "orchestrator" => Command::Orchestrator(rest.to_string()),
        "answer" | "reply" => {
            let id = first_token(rest).to_string();
            let body = rest
                .split_once(char::is_whitespace)
                .map(|x| x.1)
                .unwrap_or("")
                .trim()
                .to_string();
            Command::Answer { id, text: body }
        }
        "help" | "start" | "h" => Command::Help,
        other => Command::Unknown(other.to_string()),
    }
}

fn first_token(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

/// The first 8 chars of a task id (used everywhere in messages / callback data).
pub fn short_id(id: &str) -> &str {
    if id.len() >= 8 {
        &id[..8]
    } else {
        id
    }
}

/// The `move_task` action that advances a task from its current status, if any.
/// Mirrors the MCP server's `allowed_actions` logic.
pub fn next_action(status: TaskStatus) -> Option<&'static str> {
    match status {
        TaskStatus::Backlog => Some("move_forward"),
        TaskStatus::Planning => Some("move_forward"),
        TaskStatus::Running => Some("move_forward"),
        TaskStatus::Review => Some("move_to_done"),
        TaskStatus::Done => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Render the board as a compact, phone-friendly text message.
///
/// Lists Backlog / Planning / Running / Review with one line per task (short id + title);
/// summarizes Done as a count to keep the message short.
pub fn render_board(project_name: &str, tasks: &[Task]) -> String {
    let mut out = String::new();
    out.push_str(&format!("📋 agtx · {project_name}\n"));

    let sections = [
        ("BACKLOG", TaskStatus::Backlog),
        ("PLANNING", TaskStatus::Planning),
        ("RUNNING", TaskStatus::Running),
        ("REVIEW", TaskStatus::Review),
    ];

    for (label, status) in sections {
        let items: Vec<&Task> = tasks.iter().filter(|t| t.status == status).collect();
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{label}\n"));
        for t in items {
            out.push_str(&format!(
                "  #{} · {}\n",
                short_id(&t.id),
                truncate(&t.title, 34)
            ));
        }
    }

    let done = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    let backlog = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Backlog)
        .count();
    out.push_str(&format!("\nBACKLOG: {backlog}   DONE: {done}"));

    if tasks.is_empty() {
        out.push_str("\n\n(no tasks yet — /new <title> to create one)");
    }
    out
}

/// Help text listing all commands.
pub fn help_text() -> String {
    "\
agtx Telegram bridge — commands:

/board — show the board
/advance <id> — move a task to its next phase
/resume <id> — resume a Review task to Running
/new <title> — create a backlog task
/answer <id> <text> — answer a specific task
/select <id> — set the active task for bare replies
/orch [message] — view the orchestrator's chat, or send it a message
/help — this message

You can also just reply to a task's notification to answer it, or tap the buttons."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn task(title: &str, status: TaskStatus) -> Task {
        let mut t = Task::new(title, "claude", "proj");
        t.status = status;
        t
    }

    #[test]
    fn parses_basic_commands() {
        assert_eq!(parse_command("/board"), Command::Board);
        assert_eq!(parse_command("/b"), Command::Board);
        assert_eq!(parse_command("/help"), Command::Help);
        assert_eq!(
            parse_command("/advance a1b2c3d4"),
            Command::Advance("a1b2c3d4".to_string())
        );
        assert_eq!(
            parse_command("/resume a1b2c3d4"),
            Command::Resume("a1b2c3d4".to_string())
        );
        assert_eq!(
            parse_command("/new Add rate limiting"),
            Command::New("Add rate limiting".to_string())
        );
        assert_eq!(
            parse_command("/select a1b2c3d4"),
            Command::Select("a1b2c3d4".to_string())
        );
    }

    #[test]
    fn parses_botname_suffix() {
        assert_eq!(parse_command("/board@agtx_bot"), Command::Board);
    }

    #[test]
    fn parses_orchestrator() {
        assert_eq!(parse_command("/orch"), Command::Orchestrator(String::new()));
        assert_eq!(
            parse_command("/orch what's the status?"),
            Command::Orchestrator("what's the status?".to_string())
        );
        assert_eq!(
            parse_command("/orchestrator hello"),
            Command::Orchestrator("hello".to_string())
        );
    }

    #[test]
    fn parses_answer_with_id_and_text() {
        assert_eq!(
            parse_command("/answer a1b2c3d4 use the JSON format"),
            Command::Answer {
                id: "a1b2c3d4".to_string(),
                text: "use the JSON format".to_string()
            }
        );
    }

    #[test]
    fn unknown_command() {
        assert_eq!(
            parse_command("/frobnicate"),
            Command::Unknown("frobnicate".to_string())
        );
    }

    #[test]
    fn next_action_matches_allowed_actions() {
        assert_eq!(next_action(TaskStatus::Planning), Some("move_forward"));
        assert_eq!(next_action(TaskStatus::Running), Some("move_forward"));
        assert_eq!(next_action(TaskStatus::Review), Some("move_to_done"));
        assert_eq!(next_action(TaskStatus::Backlog), Some("move_forward"));
        assert_eq!(next_action(TaskStatus::Done), None);
    }

    #[test]
    fn renders_board_grouped() {
        let tasks = vec![
            task("Refactor auth", TaskStatus::Planning),
            task("Add retry logic", TaskStatus::Running),
            task("Fix flaky test", TaskStatus::Review),
            task("Old work", TaskStatus::Done),
            task("Idea", TaskStatus::Backlog),
        ];
        let out = render_board("myproj", &tasks);
        assert!(out.contains("PLANNING"));
        assert!(out.contains("Refactor auth"));
        assert!(out.contains("RUNNING"));
        assert!(out.contains("REVIEW"));
        assert!(out.contains("DONE: 1"));
        assert!(out.contains("BACKLOG: 1"));
    }

    #[test]
    fn renders_empty_board_hint() {
        let out = render_board("myproj", &[]);
        assert!(out.contains("no tasks yet"));
    }
}
