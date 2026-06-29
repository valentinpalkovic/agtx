//! Multi-project Telegram daemon — a single bot serving **all** agtx projects.
//!
//! This is the cross-project counterpart to the in-TUI bridge ([`super`]). It runs as a
//! standalone process (`agtx telegram-serve`) so there's exactly **one** `getUpdates`
//! poller for the bot (Telegram rejects concurrent pollers per token). It enumerates every
//! project from the global index, monitors each project's task tmux panes directly, and
//! labels every notification with the project name. Inbound replies/commands are routed
//! back to the right project.
//!
//! Run this INSTEAD of the in-TUI bridge (don't enable both — they'd both poll). Answers
//! inject straight into the task's tmux pane, so they work without that project's TUI being
//! open; phase transitions (`/advance`, `/new`, `/resume`) are queued and still need that
//! project's agtx TUI running to execute their side effects.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::GlobalConfig;
use crate::db::{Database, Task, TaskStatus, TransitionRequest};
use crate::tmux::{RealTmuxOps, TmuxOperations};

use super::api::{Button, TelegramApi};
use super::commands::{help_text, next_action, parse_command, short_id, Command};
use super::extract::{self, classify, Classification, QuestionKind};

/// How long a pane must be byte-stable before we treat the task as idle.
const IDLE_SECS: u64 = 15;

/// Entry point for `agtx telegram-serve`.
pub fn serve_daemon() -> anyhow::Result<()> {
    let cfg = GlobalConfig::load().unwrap_or_default().telegram;
    let token = cfg.resolved_token().ok_or_else(|| {
        anyhow::anyhow!(
            "Telegram not configured: set AGTX_TELEGRAM_BOT_TOKEN or [telegram].bot_token"
        )
    })?;
    if cfg.allowed_chat_ids.is_empty() {
        eprintln!("warning: [telegram].allowed_chat_ids is empty — inbound is rejected and there's nobody to notify.");
    }
    let global = Database::open_global()?;
    let mut daemon = Daemon {
        api: TelegramApi::new(token, cfg.poll_timeout_secs),
        allowed_chat_ids: cfg.allowed_chat_ids,
        poll_timeout_secs: cfg.poll_timeout_secs,
        tmux: Arc::new(RealTmuxOps),
        global,
        dbs: HashMap::new(),
        names: HashMap::new(),
        key_to_path: HashMap::new(),
        pane_hashes: HashMap::new(),
        notified: HashSet::new(),
        routes: HashMap::new(),
        active: None,
        pending: Vec::new(),
        offset: 0,
    };
    daemon.run();
    Ok(())
}

/// A pending outbound question, so a reply can be routed to the right project/task.
struct Route {
    session_name: String,
    pane_hash: u64,
    chat_id: i64,
    text: String,
}

/// A queued transition we're waiting on so we can report its outcome.
struct Pending {
    project_path: String,
    request_id: String,
    chat_id: i64,
    label: String,
    deadline: Instant,
    suggest_next: bool,
}

/// Compact inline-button payload (must stay <=64 bytes serialized): project key + short id.
#[derive(Debug, Serialize, Deserialize)]
struct CallbackData {
    /// 8-hex project key (see `project_key`).
    pk: String,
    /// Short (8-char) task id.
    t: String,
    /// Action: "send" | "keys" | "pane" | "adv".
    k: String,
    #[serde(default)]
    p: String,
}

struct Daemon {
    api: TelegramApi,
    allowed_chat_ids: Vec<i64>,
    poll_timeout_secs: u64,
    tmux: Arc<dyn TmuxOperations>,
    global: Database,
    /// project path -> cached project DB connection.
    dbs: HashMap<String, Database>,
    /// project path -> display name.
    names: HashMap<String, String>,
    /// 8-hex project key -> project path (for resolving callbacks).
    key_to_path: HashMap<String, String>,
    /// "path\0task" -> (pane hash, first-seen-stable time) for idle detection.
    pane_hashes: HashMap<String, (u64, Instant)>,
    /// "path\0task" one-shot guard for the current idle episode.
    notified: HashSet<String>,
    /// Telegram message_id -> route.
    routes: HashMap<i64, Route>,
    /// (project_path, task_id) most recently asked — target for bare replies.
    active: Option<(String, String)>,
    pending: Vec<Pending>,
    offset: i64,
}

impl Daemon {
    fn run(&mut self) {
        // Discard updates that arrived before startup.
        if let Ok(updates) = self.api.get_updates(-1, 0) {
            for u in &updates {
                if let Some(id) = u.get("update_id").and_then(|v| v.as_i64()) {
                    self.offset = self.offset.max(id + 1);
                }
            }
        }
        tracing::info!("telegram daemon: started (multi-project)");

        loop {
            self.scan_projects();

            match self.api.get_updates(self.offset, self.poll_timeout_secs) {
                Ok(updates) => {
                    for u in updates {
                        if let Some(id) = u.get("update_id").and_then(|v| v.as_i64()) {
                            self.offset = self.offset.max(id + 1);
                        }
                        self.handle_update(&u);
                    }
                }
                Err(e) => {
                    tracing::warn!("telegram daemon: getUpdates failed: {e}");
                    std::thread::sleep(Duration::from_secs(3));
                }
            }

            self.check_pending();
            self.prune_stale_routes();
        }
    }

    // ── Project / DB helpers ────────────────────────────────────────────────

    fn db_for(&mut self, path: &str) -> Option<&Database> {
        if !self.dbs.contains_key(path) {
            match Database::open_project(Path::new(path)) {
                Ok(db) => {
                    self.dbs.insert(path.to_string(), db);
                }
                Err(e) => {
                    tracing::warn!("telegram daemon: open project db {path} failed: {e}");
                    return None;
                }
            }
        }
        self.dbs.get(path)
    }

    /// (path, name) for every project, refreshing the key/name maps.
    fn projects(&mut self) -> Vec<(String, String)> {
        let list: Vec<(String, String)> = self
            .global
            .get_all_projects()
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.path, p.name))
            .collect();
        for (path, name) in &list {
            self.names.insert(path.clone(), name.clone());
            self.key_to_path.insert(project_key(path), path.clone());
        }
        list
    }

    fn name_of(&self, path: &str) -> String {
        self.names.get(path).cloned().unwrap_or_else(|| {
            Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string())
        })
    }

    fn find_task_in(&mut self, path: &str, short: &str) -> Option<Task> {
        let all = self.db_for(path)?.get_all_tasks().ok()?;
        all.into_iter()
            .find(|t| t.id.starts_with(short) || short_id(&t.id) == short)
    }

    /// Find a task by short id across all projects. Returns (path, task), or None if not
    /// found; logs/handled by callers when ambiguous.
    fn find_task_across(&mut self, short: &str) -> CrossLookup {
        let projects = self.projects();
        let mut found: Vec<(String, Task)> = Vec::new();
        for (path, _) in projects {
            if let Some(db) = self.db_for(&path) {
                if let Ok(all) = db.get_all_tasks() {
                    if let Some(t) = all
                        .into_iter()
                        .find(|t| t.id.starts_with(short) || short_id(&t.id) == short)
                    {
                        found.push((path.clone(), t));
                    }
                }
            }
        }
        match found.len() {
            0 => CrossLookup::None,
            1 => {
                let (p, t) = found.pop().unwrap();
                CrossLookup::One(p, Box::new(t))
            }
            n => CrossLookup::Many(n),
        }
    }

    fn project_by_name(&mut self, name: &str) -> Option<String> {
        let projects = self.projects();
        let lname = name.to_lowercase();
        projects
            .into_iter()
            .find(|(_, n)| n.to_lowercase() == lname)
            .map(|(p, _)| p)
    }

    // ── Outbound: scan every project for idle questions / completions ────────

    fn scan_projects(&mut self) {
        if self.allowed_chat_ids.is_empty() {
            return;
        }
        let projects = self.projects();
        for (path, _name) in projects {
            let tasks = match self.db_for(&path) {
                Some(db) => db.get_all_tasks().unwrap_or_default(),
                None => continue,
            };
            for task in tasks {
                if eligible(&task) {
                    self.scan_task(&path, &task);
                }
            }
        }
    }

    fn scan_task(&mut self, path: &str, task: &Task) {
        let Some(session) = task.session_name.clone() else {
            return;
        };
        if !self.tmux.window_exists(&session).unwrap_or(false) {
            return;
        }
        let pane = match self.tmux.capture_pane(&session) {
            Ok(p) => p,
            Err(_) => return,
        };
        let hash = hash_pane(&pane);
        let key = format!("{path}\0{}", task.id);
        let now = Instant::now();

        let entry = self.pane_hashes.entry(key.clone()).or_insert((hash, now));
        if entry.0 != hash {
            // Pane changed — reset idle timer and re-arm notifications.
            *entry = (hash, now);
            self.notified.remove(&key);
            return;
        }
        if now.duration_since(entry.1) < Duration::from_secs(IDLE_SECS) {
            return; // not idle long enough yet
        }
        if self.notified.contains(&key) {
            return; // already handled this idle episode
        }

        let phase = phase_label(task.status);
        match classify(&pane, &task.agent) {
            Classification::Asking { kind, options, .. } => {
                self.notify_question(path, task, &session, phase, kind, options);
                self.notified.insert(key);
            }
            Classification::Finished => {
                // Completion ping only for phases that need a human decision.
                if matches!(phase, "running" | "review" | "research") {
                    self.notify_completion(path, task, phase);
                    self.notified.insert(key);
                }
            }
            Classification::Stuck => {}
        }
    }

    fn notify_question(
        &mut self,
        path: &str,
        task: &Task,
        session: &str,
        phase: &str,
        kind: QuestionKind,
        options: Vec<extract::ExtractedOption>,
    ) {
        let name = self.name_of(path);
        let short = short_id(&task.id).to_string();
        let pk = project_key(path);
        // Full message via the ⏺ marker from scrollback; fall back to visible pane.
        let history =
            String::from_utf8_lossy(&self.tmux.capture_pane_with_history(session, 500)).to_string();
        let body = extract::extract_marked_message(&history, 200, 3600)
            .unwrap_or_else(|| extract::clean_pane(&history, 25, 1500));
        let pane_hash = self
            .tmux
            .capture_pane(session)
            .map(|p| hash_pane(&p))
            .unwrap_or(0);

        let mut text = format!(
            "🟡 {name} · #{short} · {phase} · {}\n{}\n\n{}",
            task.agent, task.title, body
        );
        match kind {
            QuestionKind::FreeText => text.push_str("\n\n💬 Reply to this message to answer."),
            _ => text.push_str("\n\n👇 Tap an option below."),
        }
        let keyboard = build_keyboard(&pk, &short, &kind, &options);

        for &chat_id in &self.allowed_chat_ids {
            if let Ok(message_id) =
                self.api
                    .send_message(chat_id, &text, None, Some(keyboard.clone()))
            {
                self.routes.insert(
                    message_id,
                    Route {
                        session_name: session.to_string(),
                        pane_hash,
                        chat_id,
                        text: text.clone(),
                    },
                );
            }
        }
        self.active = Some((path.to_string(), task.id.clone()));
    }

    fn notify_completion(&mut self, path: &str, task: &Task, phase: &str) {
        let name = self.name_of(path);
        let short = short_id(&task.id).to_string();
        let pk = project_key(path);
        let action_label = match task.status {
            TaskStatus::Review => "✅ Mark done",
            _ => "⏭ Advance",
        };
        let mut text = format!(
            "✅ {name} · #{short} · {phase} · {}\n{}",
            task.agent, task.title
        );
        if let Some(url) = task.pr_url.as_deref().filter(|u| !u.is_empty()) {
            text.push_str(&format!("\nPR: {url}"));
        }
        text.push_str("\n\nWork looks complete — your call:");
        let rows = vec![
            vec![Button {
                text: action_label.to_string(),
                callback_data: cb(&pk, &short, "adv", ""),
            }],
            vec![Button {
                text: "📋 Show pane".to_string(),
                callback_data: cb(&pk, &short, "pane", ""),
            }],
        ];
        for &chat_id in &self.allowed_chat_ids {
            let _ = self
                .api
                .send_message(chat_id, &text, None, Some(rows.clone()));
        }
        self.active = Some((path.to_string(), task.id.clone()));
    }

    // ── Inbound ─────────────────────────────────────────────────────────────

    fn handle_update(&mut self, u: &serde_json::Value) {
        if let Some(cq) = u.get("callback_query") {
            self.handle_callback(cq);
        } else if let Some(msg) = u.get("message") {
            self.handle_message(msg);
        }
    }

    fn handle_callback(&mut self, cq: &serde_json::Value) {
        let chat_id = cq
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_i64());
        let cb_id = cq.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let message_id = cq
            .get("message")
            .and_then(|m| m.get("message_id"))
            .and_then(|v| v.as_i64());
        let Some(chat_id) = chat_id else { return };
        if !self.is_authorized(chat_id) {
            let _ = self.api.answer_callback_query(cb_id, Some("Unauthorized"));
            return;
        }
        let data: Option<CallbackData> = cq
            .get("data")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok());
        let Some(data) = data else {
            let _ = self.api.answer_callback_query(cb_id, Some("Bad button"));
            return;
        };
        let Some(path) = self.key_to_path.get(&data.pk).cloned() else {
            let _ = self
                .api
                .answer_callback_query(cb_id, Some("Unknown project"));
            return;
        };
        let Some(task) = self.find_task_in(&path, &data.t) else {
            let _ = self
                .api
                .answer_callback_query(cb_id, Some("Task not found"));
            return;
        };

        match data.k.as_str() {
            "pane" => {
                let _ = self.api.answer_callback_query(cb_id, None);
                self.send_pane(chat_id, &task, message_id);
            }
            "adv" => {
                let _ = self.api.answer_callback_query(cb_id, Some("Advancing…"));
                self.enqueue_advance(chat_id, &path, &task);
            }
            "send" | "keys" => {
                let Some(session) = task.session_name.clone() else {
                    let _ = self.api.answer_callback_query(cb_id, Some("No session"));
                    return;
                };
                if !self.tmux.window_exists(&session).unwrap_or(false) {
                    let _ = self.api.answer_callback_query(cb_id, Some("Session gone"));
                    return;
                }
                if let Some(mid) = message_id {
                    if let Some(route) = self.routes.get(&mid) {
                        let now_hash = self
                            .tmux
                            .capture_pane(&session)
                            .map(|p| hash_pane(&p))
                            .unwrap_or(route.pane_hash);
                        if now_hash != route.pane_hash {
                            let _ = self
                                .api
                                .answer_callback_query(cb_id, Some("Question changed"));
                            self.send(chat_id, "⚠️ That question is no longer on screen.");
                            self.routes.remove(&mid);
                            return;
                        }
                    }
                }
                inject_keys(self.tmux.as_ref(), &session, &data.k, &data.p);
                let _ = self
                    .api
                    .answer_callback_query(cb_id, Some("✅ Answer sent"));
                if let Some(mid) = message_id {
                    if let Some(route) = self.routes.remove(&mid) {
                        let edited = format!("{}\n\n✅ You answered: {}", route.text, data.p);
                        let _ = self.api.edit_message_text(route.chat_id, mid, &edited);
                    }
                }
            }
            _ => {
                let _ = self
                    .api
                    .answer_callback_query(cb_id, Some("Unknown action"));
            }
        }
    }

    fn handle_message(&mut self, msg: &serde_json::Value) {
        let chat_id = msg
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_i64());
        let Some(chat_id) = chat_id else { return };
        if !self.is_authorized(chat_id) {
            return;
        }
        let text = msg
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }
        let reply_to = msg
            .get("reply_to_message")
            .and_then(|m| m.get("message_id"))
            .and_then(|v| v.as_i64());

        if text.starts_with('/') {
            self.handle_command(chat_id, parse_command(&text));
        } else {
            self.handle_free_text(chat_id, &text, reply_to);
        }
    }

    fn handle_free_text(&mut self, chat_id: i64, text: &str, reply_to: Option<i64>) {
        // reply-to a question message > active task.
        let target: Option<(String, Option<i64>)> = reply_to
            .and_then(|mid| {
                self.routes
                    .get(&mid)
                    .map(|r| (r.session_name.clone(), Some(mid)))
            })
            .or_else(|| {
                self.active.clone().and_then(|(path, tid)| {
                    self.find_task_in(&path, &tid)
                        .and_then(|t| t.session_name.map(|s| (s, None)))
                })
            });
        let Some((session, route_mid)) = target else {
            self.send(
                chat_id,
                "Reply to a task's message, or use /answer <id> <text> or /select <id>.",
            );
            return;
        };
        if !self.tmux.window_exists(&session).unwrap_or(false) {
            self.send(chat_id, "That task's session is no longer running.");
            return;
        }
        inject_text(self.tmux.as_ref(), &session, text);
        match route_mid.and_then(|mid| self.routes.remove(&mid).map(|r| (mid, r))) {
            Some((mid, route)) => {
                let edited = format!("{}\n\n✅ You answered: {text}", route.text);
                let _ = self.api.edit_message_text(route.chat_id, mid, &edited);
            }
            None => self.send(chat_id, "✅ Sent."),
        }
    }

    fn handle_command(&mut self, chat_id: i64, cmd: Command) {
        match cmd {
            Command::Board => self.send_board(chat_id),
            Command::Advance(short) => match self.find_task_across(&short) {
                CrossLookup::One(path, task) => self.enqueue_advance(chat_id, &path, &task),
                CrossLookup::None => self.send(chat_id, &format!("No task matching #{short}")),
                CrossLookup::Many(n) => self.send(
                    chat_id,
                    &format!("#{short} matches {n} tasks across projects — be more specific."),
                ),
            },
            Command::Resume(short) => match self.find_task_across(&short) {
                CrossLookup::One(path, task) if task.status == TaskStatus::Review => {
                    self.enqueue_transition(chat_id, &path, &task, "resume", "resuming", false);
                }
                CrossLookup::One(_, _) => self.send(chat_id, "Only Review tasks can be resumed."),
                CrossLookup::None => self.send(chat_id, &format!("No task matching #{short}")),
                CrossLookup::Many(n) => self.send(
                    chat_id,
                    &format!("#{short} matches {n} tasks across projects — be more specific."),
                ),
            },
            Command::Answer { id, text } => match self.find_task_across(&id) {
                CrossLookup::One(_, task) => match task.session_name.clone() {
                    Some(s) if self.tmux.window_exists(&s).unwrap_or(false) => {
                        inject_text(self.tmux.as_ref(), &s, &text);
                        self.send(chat_id, "✅ Sent.");
                    }
                    _ => self.send(chat_id, "That task has no running session."),
                },
                CrossLookup::None => self.send(chat_id, &format!("No task matching #{id}")),
                CrossLookup::Many(n) => self.send(
                    chat_id,
                    &format!("#{id} matches {n} tasks across projects — be more specific."),
                ),
            },
            Command::Select(short) => match self.find_task_across(&short) {
                CrossLookup::One(path, task) => {
                    let st = short_id(&task.id).to_string();
                    let title = task.title.clone();
                    self.active = Some((path, task.id));
                    self.send(chat_id, &format!("Active task set to #{st}: {title}"));
                }
                CrossLookup::None => self.send(chat_id, &format!("No task matching #{short}")),
                CrossLookup::Many(n) => self.send(
                    chat_id,
                    &format!("#{short} matches {n} tasks across projects — be more specific."),
                ),
            },
            Command::New(rest) => self.handle_new(chat_id, &rest),
            Command::Orchestrator(rest) => self.handle_orch(chat_id, &rest),
            Command::Help => self.send(
                chat_id,
                &format!(
                    "{}\n\n(multi-project mode: /new and /orch take a <project> first token)",
                    help_text()
                ),
            ),
            Command::Unknown(c) => self.send(chat_id, &format!("Unknown command: /{c}\nTry /help")),
        }
    }

    fn handle_new(&mut self, chat_id: i64, rest: &str) {
        // /new <project> <title>
        let mut parts = rest.trim().splitn(2, char::is_whitespace);
        let proj = parts.next().unwrap_or("").trim();
        let title = parts.next().unwrap_or("").trim();
        let Some(path) = self.project_by_name(proj) else {
            self.send(chat_id, "Usage: /new <project> <title>");
            return;
        };
        if title.is_empty() {
            self.send(chat_id, "Usage: /new <project> <title>");
            return;
        }
        let global = GlobalConfig::load().unwrap_or_default();
        let project = crate::config::ProjectConfig::load(Path::new(&path)).unwrap_or_default();
        let merged = crate::config::MergedConfig::merge(&global, &project);
        let pk = project_key(&path);
        let (project_id, plugin, agent) = {
            let Some(db) = self.db_for(&path) else {
                self.send(chat_id, "Couldn't open that project.");
                return;
            };
            let existing = db.get_all_tasks().unwrap_or_default();
            let project_id = existing
                .first()
                .map(|t| t.project_id.clone())
                .unwrap_or_else(|| self.name_of(&path));
            (
                project_id,
                merged.workflow_plugin.clone(),
                merged.default_agent.clone(),
            )
        };
        let mut task = Task::new(title, agent.clone(), project_id);
        task.plugin = plugin;
        let short = short_id(&task.id).to_string();
        let created = self.db_for(&path).map(|db| db.create_task(&task));
        match created {
            Some(Ok(())) => {
                let kb = vec![vec![Button {
                    text: "▶️ Start".to_string(),
                    callback_data: cb(&pk, &short, "adv", ""),
                }]];
                let _ = self.api.send_message(
                    chat_id,
                    &format!(
                        "✅ Created #{short} \"{title}\" in {} backlog ({agent}).",
                        self.name_of(&path)
                    ),
                    None,
                    Some(kb),
                );
            }
            _ => self.send(chat_id, "Failed to create task."),
        }
    }

    fn handle_orch(&mut self, chat_id: i64, rest: &str) {
        // /orch <project> [message]
        let mut parts = rest.trim().splitn(2, char::is_whitespace);
        let proj = parts.next().unwrap_or("").trim();
        let msg = parts.next().unwrap_or("").trim().to_string();
        let Some(path) = self.project_by_name(proj) else {
            self.send(chat_id, "Usage: /orch <project> [message]");
            return;
        };
        let target = format!(
            "{}:orchestrator",
            crate::tmux::safe_session_name(&self.name_of(&path))
        );
        if !self.tmux.window_exists(&target).unwrap_or(false) {
            self.send(
                chat_id,
                "Orchestrator isn't running for that project (toggle with O in agtx, --experimental).",
            );
            return;
        }
        if msg.is_empty() {
            self.send_orch_pane(chat_id, &target);
        } else {
            let _ = self.tmux.send_keys(&target, &msg);
            self.send(chat_id, "📨 Sent to the orchestrator.");
            // Show its reply shortly after (best-effort, synchronous small wait).
            std::thread::sleep(Duration::from_secs(3));
            self.send_orch_pane(chat_id, &target);
        }
    }

    fn send_orch_pane(&self, chat_id: i64, target: &str) {
        let content = self.tmux.capture_pane(target).unwrap_or_default();
        let body = extract::clean_pane(&content, 40, 3500);
        let text = if body.is_empty() {
            "🧭 Orchestrator (no visible output yet)".to_string()
        } else {
            format!("🧭 Orchestrator:\n\n{body}")
        };
        self.send(chat_id, &text);
    }

    fn send_pane(&self, chat_id: i64, task: &Task, reply_to: Option<i64>) {
        let Some(session) = &task.session_name else {
            self.send(chat_id, "That task has no session.");
            return;
        };
        let content = self.tmux.capture_pane(session).unwrap_or_default();
        let excerpt = extract::clean_pane(&content, 30, 3500);
        let text = format!("📋 #{}\n\n{}", short_id(&task.id), excerpt);
        let _ = self.api.send_message(chat_id, &text, reply_to, None);
    }

    fn send_board(&mut self, chat_id: i64) {
        let projects = self.projects();
        let mut out = String::from("📋 agtx — all projects\n");
        let mut any = false;
        for (path, name) in projects {
            let tasks = match self.db_for(&path) {
                Some(db) => db.get_all_tasks().unwrap_or_default(),
                None => continue,
            };
            let active: Vec<&Task> = tasks
                .iter()
                .filter(|t| {
                    matches!(
                        t.status,
                        TaskStatus::Planning | TaskStatus::Running | TaskStatus::Review
                    )
                })
                .collect();
            let backlog = tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Backlog)
                .count();
            if active.is_empty() && backlog == 0 {
                continue;
            }
            any = true;
            out.push_str(&format!("\n*{name}*\n"));
            for t in active {
                out.push_str(&format!(
                    "  #{} · {} · {}\n",
                    short_id(&t.id),
                    t.status.as_str(),
                    truncate(&t.title, 30)
                ));
            }
            if backlog > 0 {
                out.push_str(&format!("  backlog: {backlog}\n"));
            }
        }
        if !any {
            out.push_str("\n(no active tasks)");
        }
        self.send(chat_id, &out);
    }

    // ── Transitions ─────────────────────────────────────────────────────────

    fn enqueue_advance(&mut self, chat_id: i64, path: &str, task: &Task) {
        let Some(action) = next_action(task.status) else {
            self.send(
                chat_id,
                "This task can't be advanced from its current phase.",
            );
            return;
        };
        if task.status == TaskStatus::Backlog {
            let satisfied = self
                .db_for(path)
                .map(|db| db.deps_satisfied(task))
                .unwrap_or(true);
            if !satisfied {
                self.send(chat_id, "Blocked: dependencies aren't satisfied yet.");
                return;
            }
        }
        let verb = if action == "move_to_done" {
            "moving to done"
        } else {
            "advancing"
        };
        let suggest_next = action == "move_to_done";
        self.enqueue_transition(chat_id, path, task, action, verb, suggest_next);
    }

    fn enqueue_transition(
        &mut self,
        chat_id: i64,
        path: &str,
        task: &Task,
        action: &str,
        verb: &str,
        suggest_next: bool,
    ) {
        let req = TransitionRequest::new(task.id.clone(), action.to_string());
        let request_id = req.id.clone();
        let short = short_id(&task.id).to_string();
        let name = self.name_of(path);
        let label = format!("{verb} {name} #{short}");
        let status = task.status.as_str().to_string();
        let created = self
            .db_for(path)
            .map(|db| db.create_transition_request(&req));
        match created {
            Some(Ok(())) => {
                self.send(chat_id, &format!("⏳ {label} ({status})…"));
                self.pending.push(Pending {
                    project_path: path.to_string(),
                    request_id,
                    chat_id,
                    label,
                    deadline: Instant::now() + Duration::from_secs(90),
                    suggest_next,
                });
            }
            _ => self.send(chat_id, &format!("Failed to queue {label}.")),
        }
    }

    fn check_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let drained: Vec<Pending> = self.pending.drain(..).collect();
        let mut still = Vec::new();
        for p in drained {
            let status = self
                .db_for(&p.project_path)
                .and_then(|db| db.get_transition_request(&p.request_id).ok().flatten());
            match status {
                Some(req) if req.processed_at.is_some() => match req.error {
                    Some(err) => self.send(p.chat_id, &format!("❌ {}: {err}", p.label)),
                    None => {
                        self.send(p.chat_id, &format!("✅ {} — done.", p.label));
                        if p.suggest_next {
                            self.suggest_next_task(p.chat_id, &p.project_path);
                        }
                    }
                },
                _ => {
                    if Instant::now() >= p.deadline {
                        self.send(
                            p.chat_id,
                            &format!(
                                "⏳ {} is still queued — is that project's agtx TUI running?",
                                p.label
                            ),
                        );
                    } else {
                        still.push(p);
                    }
                }
            }
        }
        self.pending = still;
    }

    fn suggest_next_task(&mut self, chat_id: i64, path: &str) {
        let pk = project_key(path);
        let next = self.db_for(path).and_then(|db| {
            let tasks = db.get_all_tasks().unwrap_or_default();
            tasks
                .into_iter()
                .find(|t| t.status == TaskStatus::Backlog && db.deps_satisfied(t))
        });
        match next {
            Some(t) => {
                let short = short_id(&t.id).to_string();
                let kb = vec![vec![Button {
                    text: "▶️ Start".to_string(),
                    callback_data: cb(&pk, &short, "adv", ""),
                }]];
                let _ = self.api.send_message(
                    chat_id,
                    &format!("Next up in {}: #{short} {}", self.name_of(path), t.title),
                    None,
                    Some(kb),
                );
            }
            None => self.send(chat_id, "No backlog tasks are ready to start."),
        }
    }

    // ── Maintenance ─────────────────────────────────────────────────────────

    fn prune_stale_routes(&mut self) {
        if self.routes.is_empty() {
            return;
        }
        let mut stale: Vec<(i64, i64)> = Vec::new();
        for (&message_id, route) in &self.routes {
            let gone = !self
                .tmux
                .window_exists(&route.session_name)
                .unwrap_or(false);
            let changed = !gone
                && self
                    .tmux
                    .capture_pane(&route.session_name)
                    .map(|p| hash_pane(&p) != route.pane_hash)
                    .unwrap_or(false);
            if gone || changed {
                stale.push((message_id, route.chat_id));
            }
        }
        for (message_id, chat_id) in stale {
            let _ = self.api.delete_message(chat_id, message_id);
            self.routes.remove(&message_id);
        }
    }

    fn is_authorized(&self, chat_id: i64) -> bool {
        !self.allowed_chat_ids.is_empty() && self.allowed_chat_ids.contains(&chat_id)
    }

    fn send(&self, chat_id: i64, text: &str) {
        if let Err(e) = self.api.send_message(chat_id, text, None, None) {
            tracing::warn!("telegram daemon: send failed: {e}");
        }
    }
}

enum CrossLookup {
    None,
    One(String, Box<Task>),
    /// Matched in N projects — ambiguous.
    Many(usize),
}

fn eligible(task: &Task) -> bool {
    matches!(
        task.status,
        TaskStatus::Planning | TaskStatus::Running | TaskStatus::Review
    ) || (task.status == TaskStatus::Backlog && task.session_name.is_some())
}

fn phase_label(status: TaskStatus) -> &'static str {
    if status == TaskStatus::Backlog {
        "research"
    } else {
        status.as_str()
    }
}

fn inject_text(tmux: &dyn TmuxOperations, session: &str, text: &str) {
    if text.contains('\n') {
        let _ = tmux.paste_text(session, text);
        let _ = tmux.send_keys_literal(session, "Enter");
    } else {
        let _ = tmux.send_keys(session, text);
    }
}

fn inject_keys(tmux: &dyn TmuxOperations, session: &str, kind: &str, payload: &str) {
    match kind {
        "send" => {
            let _ = tmux.send_keys(session, payload);
        }
        "keys" => {
            for key in payload.split_whitespace() {
                let _ = tmux.send_keys_literal(session, key);
            }
        }
        _ => {}
    }
}

fn build_keyboard(
    pk: &str,
    short: &str,
    kind: &QuestionKind,
    options: &[extract::ExtractedOption],
) -> Vec<Vec<Button>> {
    let mut rows: Vec<Vec<Button>> = Vec::new();
    match kind {
        QuestionKind::Menu => {
            for opt in options {
                rows.push(vec![Button {
                    text: format!("{} · {}", opt.key, truncate(&opt.label, 40)),
                    callback_data: cb(pk, short, "send", &opt.key),
                }]);
            }
        }
        QuestionKind::YesNo => {
            rows.push(vec![
                Button {
                    text: "✅ Yes".to_string(),
                    callback_data: cb(pk, short, "send", "y"),
                },
                Button {
                    text: "❌ No".to_string(),
                    callback_data: cb(pk, short, "send", "n"),
                },
            ]);
        }
        QuestionKind::FreeText => {}
    }
    rows.push(vec![Button {
        text: "📋 Show pane".to_string(),
        callback_data: cb(pk, short, "pane", ""),
    }]);
    rows
}

fn cb(pk: &str, short: &str, k: &str, p: &str) -> String {
    serde_json::to_string(&CallbackData {
        pk: pk.to_string(),
        t: short.to_string(),
        k: k.to_string(),
        p: p.to_string(),
    })
    .unwrap_or_default()
}

fn hash_pane(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Stable 8-hex project key derived from the project path (fits Telegram callback_data).
fn project_key(path: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    let r = h.finalize();
    format!("{:02x}{:02x}{:02x}{:02x}", r[0], r[1], r[2], r[3])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_data_with_project_fits_limit() {
        let s = cb("1a2b3c4d", "a1b2c3d4", "send", "2");
        assert!(s.len() <= 64, "callback too long: {} bytes", s.len());
    }

    #[test]
    fn project_key_is_stable_8_hex() {
        let k = project_key("/Users/me/projects/foo");
        assert_eq!(k.len(), 8);
        assert_eq!(k, project_key("/Users/me/projects/foo"));
        assert_ne!(k, project_key("/Users/me/projects/bar"));
    }
}
