//! Telegram bridge: notify on idle agent questions, answer/control from your phone.
//!
//! Runs as a single background thread inside the agtx TUI process. The TUI's
//! `apply_session_refresh` sends an [`OutboundCheck`] over an mpsc channel whenever a task
//! goes idle waiting for input; the bridge captures the pane, decides whether the agent is
//! actually asking something ([`extract::classify`]), and pushes a Telegram message. Inbound
//! replies/commands are handled in the same loop: answers are injected directly into the
//! task's tmux pane (works in any phase), while phase transitions are queued on the
//! `transition_requests` table for the TUI's main loop to execute with full side effects.

pub mod api;
pub mod commands;
pub mod daemon;
pub mod extract;

pub use daemon::serve_daemon;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::db::{Database, Task, TaskStatus, TransitionRequest};
use crate::tmux::TmuxOperations;

use api::{Button, TelegramApi};
use commands::{help_text, next_action, parse_command, render_board, short_id, Command};
use extract::{classify, Classification, QuestionKind};

/// A request from the TUI to check whether an idle task is asking a question.
pub struct OutboundCheck {
    pub task_id: String,
    pub session_name: String,
    pub title: String,
    /// Phase display string (e.g. "running").
    pub phase: String,
    pub agent: String,
}

/// Compact payload carried by inline-keyboard buttons (must stay <=64 bytes when serialized).
#[derive(Debug, Serialize, Deserialize)]
struct CallbackData {
    /// Short (8-char) task id.
    t: String,
    /// Action: "send" (text+Enter), "keys" (named keys, no Enter), "pane", or "adv".
    k: String,
    /// Payload for the action.
    #[serde(default)]
    p: String,
}

/// Tracks an outbound question so a reply can be routed back to the right task.
struct RouteEntry {
    session_name: String,
    pane_hash: u64,
    /// Chat the question was sent to (needed to edit/delete the message).
    chat_id: i64,
    /// Original message text, so we can append the answer when it's resolved.
    text: String,
}

/// A queued transition we're waiting on so we can report the outcome back to Telegram.
struct PendingTransition {
    request_id: String,
    chat_id: i64,
    label: String,
    deadline: Instant,
    /// When the transition completes, suggest the next backlog task (used for move_to_done).
    suggest_next: bool,
}

/// Spawn the bridge thread. Returns the sender the TUI uses to request outbound checks.
pub fn spawn(
    token: String,
    allowed_chat_ids: Vec<i64>,
    poll_timeout_secs: u64,
    project_path: PathBuf,
    tmux_ops: Arc<dyn TmuxOperations>,
) -> Sender<OutboundCheck> {
    let (tx, rx) = mpsc::channel::<OutboundCheck>();
    std::thread::Builder::new()
        .name("agtx-telegram".to_string())
        .spawn(move || {
            run_bridge(
                rx,
                token,
                allowed_chat_ids,
                poll_timeout_secs,
                project_path,
                tmux_ops,
            );
        })
        .ok();
    tx
}

struct Bridge {
    api: TelegramApi,
    db: Database,
    tmux: Arc<dyn TmuxOperations>,
    allowed_chat_ids: Vec<i64>,
    poll_timeout_secs: u64,
    project_name: String,
    project_path: PathBuf,
    routes: HashMap<i64, RouteEntry>,
    active_task: Option<String>,
    pending: Vec<PendingTransition>,
    offset: i64,
    /// Reconstructed orchestrator tmux target ("{session}:orchestrator").
    orch_target: String,
    /// Deferred capture of the orchestrator pane after a chat message: (chat_id, when).
    pending_orch_capture: Option<(i64, Instant)>,
}

fn run_bridge(
    rx: mpsc::Receiver<OutboundCheck>,
    token: String,
    allowed_chat_ids: Vec<i64>,
    poll_timeout_secs: u64,
    project_path: PathBuf,
    tmux_ops: Arc<dyn TmuxOperations>,
) {
    let db = match Database::open_project(&project_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("telegram bridge: failed to open project db: {e}");
            return;
        }
    };
    let project_name = project_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    // The orchestrator runs as window "orchestrator" in the project's tmux session.
    // The session name is the same safe-slug the TUI computes from the project name.
    let orch_target = format!(
        "{}:orchestrator",
        crate::tmux::safe_session_name(&project_name)
    );

    let mut bridge = Bridge {
        api: TelegramApi::new(token, poll_timeout_secs),
        db,
        tmux: tmux_ops,
        allowed_chat_ids,
        poll_timeout_secs,
        project_name,
        project_path,
        routes: HashMap::new(),
        active_task: None,
        pending: Vec::new(),
        offset: 0,
        orch_target,
        pending_orch_capture: None,
    };

    // Discard any updates that arrived before startup so we don't replay stale commands.
    if let Ok(updates) = bridge.api.get_updates(-1, 0) {
        for u in &updates {
            if let Some(id) = u.get("update_id").and_then(|v| v.as_i64()) {
                bridge.offset = bridge.offset.max(id + 1);
            }
        }
    }
    tracing::info!(
        "telegram bridge: started for project {}",
        bridge.project_name
    );

    loop {
        // 1. Drain outbound checks from the TUI (non-blocking). Disconnect => TUI gone.
        loop {
            match rx.try_recv() {
                Ok(check) => bridge.handle_outbound(check),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    tracing::info!("telegram bridge: TUI disconnected, exiting");
                    return;
                }
            }
        }

        // 2. Long-poll for inbound updates.
        match bridge
            .api
            .get_updates(bridge.offset, bridge.poll_timeout_secs)
        {
            Ok(updates) => {
                for u in updates {
                    if let Some(id) = u.get("update_id").and_then(|v| v.as_i64()) {
                        bridge.offset = bridge.offset.max(id + 1);
                    }
                    bridge.handle_update(&u);
                }
            }
            Err(e) => {
                tracing::warn!("telegram bridge: getUpdates failed: {e}");
                std::thread::sleep(Duration::from_secs(3));
            }
        }

        // 3. Report on any completed transitions.
        bridge.check_pending();

        // 4. Deferred orchestrator pane capture after a chat message (non-blocking).
        if let Some((chat_id, when)) = bridge.pending_orch_capture {
            if Instant::now() >= when {
                bridge.pending_orch_capture = None;
                bridge.send_orch_pane(chat_id);
            }
        }

        // 5. Delete question messages whose pane moved on (answered in the terminal).
        bridge.prune_stale_routes();
    }
}

impl Bridge {
    fn is_authorized(&self, chat_id: i64) -> bool {
        !self.allowed_chat_ids.is_empty() && self.allowed_chat_ids.contains(&chat_id)
    }

    fn send(&self, chat_id: i64, text: &str) {
        if let Err(e) = self.api.send_message(chat_id, text, None, None) {
            tracing::warn!("telegram bridge: send failed: {e}");
        }
    }

    fn find_task(&self, short: &str) -> Option<Task> {
        if short.is_empty() {
            return None;
        }
        let all = self.db.get_all_tasks().ok()?;
        all.into_iter()
            .find(|t| t.id.starts_with(short) || short_id(&t.id) == short)
    }

    // ── Outbound: idle question → Telegram ──────────────────────────────────

    fn handle_outbound(&mut self, check: OutboundCheck) {
        if self.allowed_chat_ids.is_empty() {
            return; // nowhere to send
        }
        if !self
            .tmux
            .window_exists(&check.session_name)
            .unwrap_or(false)
        {
            return;
        }
        let pane = match self.tmux.capture_pane(&check.session_name) {
            Ok(p) => p,
            Err(_) => return,
        };
        let (context, kind, options) = match classify(&pane, &check.agent) {
            Classification::Asking {
                context,
                kind,
                options,
            } => (context, kind, options),
            // Work looks done (idle at its prompt, nothing to ask). For phases that need a
            // human decision, send a completion ping instead of staying silent.
            Classification::Finished => {
                if matches!(check.phase.as_str(), "running" | "review" | "research") {
                    self.send_completion_ping(&check);
                }
                return;
            }
            // No input box on screen — mid-thought or crashed. Stay silent.
            Classification::Stuck => return,
        };

        let pane_hash = hash_pane(&pane);
        let short = short_id(&check.task_id).to_string();
        // Prefer the full last message delimited by Claude's "big dot" marker, pulled from
        // scrollback so long messages aren't cut. Fall back to the visible-window context
        // (e.g. for agents that don't print the marker).
        let history = String::from_utf8_lossy(
            &self
                .tmux
                .capture_pane_with_history(&check.session_name, 500),
        )
        .to_string();
        let body = extract::extract_marked_message(&history, 200, 3800).unwrap_or(context);
        let text = format_question(&check, &body, &kind);
        let keyboard = build_keyboard(&short, &kind, &options);

        for &chat_id in &self.allowed_chat_ids {
            match self
                .api
                .send_message(chat_id, &text, None, Some(keyboard.clone()))
            {
                Ok(message_id) => {
                    self.routes.insert(
                        message_id,
                        RouteEntry {
                            session_name: check.session_name.clone(),
                            pane_hash,
                            chat_id,
                            text: text.clone(),
                        },
                    );
                }
                Err(e) => tracing::warn!("telegram bridge: outbound send failed: {e}"),
            }
        }
        // Bare replies route to the most recently asked task.
        self.active_task = Some(check.task_id.clone());
    }

    /// Send a "work looks complete" ping for a Running/Review task awaiting a human
    /// decision, with the PR link (if any) and a Mark-done / Advance action button.
    fn send_completion_ping(&mut self, check: &OutboundCheck) {
        let task = match self.find_task(&check.task_id) {
            Some(t) => t,
            None => return,
        };
        let short = short_id(&task.id).to_string();
        let action_label = match task.status {
            TaskStatus::Review => "✅ Mark done",
            _ => "⏭ Advance",
        };
        let mut text = format!(
            "✅ #{} · {} · {}\n{}",
            short,
            task.status.as_str(),
            task.agent,
            task.title
        );
        if let Some(url) = task.pr_url.as_deref().filter(|u| !u.is_empty()) {
            text.push_str(&format!("\nPR: {url}"));
        }
        text.push_str("\n\nWork looks complete — your call:");
        let rows = vec![
            vec![Button {
                text: action_label.to_string(),
                callback_data: cb(&short, "adv", ""),
            }],
            vec![Button {
                text: "📋 Show pane".to_string(),
                callback_data: cb(&short, "pane", ""),
            }],
        ];
        for &chat_id in &self.allowed_chat_ids {
            let _ = self
                .api
                .send_message(chat_id, &text, None, Some(rows.clone()));
        }
        self.active_task = Some(task.id);
    }

    // ── Inbound: updates ────────────────────────────────────────────────────

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

        let Some(task) = self.find_task(&data.t) else {
            let _ = self
                .api
                .answer_callback_query(cb_id, Some("Task not found"));
            return;
        };

        match data.k.as_str() {
            "pane" => {
                let _ = self.api.answer_callback_query(cb_id, None);
                self.send_pane_excerpt(chat_id, &task, message_id);
            }
            "adv" => {
                let _ = self.api.answer_callback_query(cb_id, Some("Advancing…"));
                self.enqueue_advance(chat_id, &task);
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
                // Staleness guard: if the pane moved on since we asked, refuse.
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
                            self.send(
                                chat_id,
                                "⚠️ That question is no longer on screen — the agent moved on.",
                            );
                            self.routes.remove(&mid);
                            return;
                        }
                    }
                }
                self.inject_keys(&session, &data.k, &data.p);
                let _ = self
                    .api
                    .answer_callback_query(cb_id, Some("✅ Answer sent"));
                // Persist the answer in the message and remove the buttons so it reads as
                // handled and can't be re-tapped.
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
            return; // silent — don't reveal the bot to strangers
        }
        let text = msg
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            return;
        }
        let reply_to = msg
            .get("reply_to_message")
            .and_then(|m| m.get("message_id"))
            .and_then(|v| v.as_i64());

        if text.starts_with('/') {
            let cmd = parse_command(text);
            self.handle_command(chat_id, cmd);
        } else {
            self.handle_free_text(chat_id, text, reply_to);
        }
    }

    fn handle_free_text(&mut self, chat_id: i64, text: &str, reply_to: Option<i64>) {
        // Resolve the target: a reply to a known question message (so we can annotate it),
        // else the active-task pointer. Returns (session, message_id-if-routed).
        let resolved: Option<(String, Option<i64>)> = reply_to
            .and_then(|mid| {
                self.routes
                    .get(&mid)
                    .map(|r| (r.session_name.clone(), Some(mid)))
            })
            .or_else(|| {
                self.active_task
                    .as_ref()
                    .and_then(|tid| self.find_task(tid))
                    .and_then(|t| t.session_name.map(|s| (s, None)))
            });

        let Some((session, route_mid)) = resolved else {
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
        self.inject_text(&session, text);
        // If this answered a specific question message, annotate it and drop the route;
        // otherwise just confirm.
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
            Command::Board => {
                let tasks = self.db.get_all_tasks().unwrap_or_default();
                self.send(chat_id, &render_board(&self.project_name, &tasks));
            }
            Command::Advance(short) => match self.find_task(&short) {
                Some(task) => self.enqueue_advance(chat_id, &task),
                None => self.send(chat_id, &format!("No task matching #{short}")),
            },
            Command::Resume(short) => match self.find_task(&short) {
                Some(task) if task.status == TaskStatus::Review => {
                    self.enqueue_transition(chat_id, &task, "resume", "resuming", false);
                }
                Some(_) => self.send(chat_id, "Only Review tasks can be resumed."),
                None => self.send(chat_id, &format!("No task matching #{short}")),
            },
            Command::New(title) => self.handle_new(chat_id, &title),
            Command::Select(short) => match self.find_task(&short) {
                Some(task) => {
                    let st = short_id(&task.id).to_string();
                    self.active_task = Some(task.id);
                    self.send(
                        chat_id,
                        &format!(
                            "Active task set to #{st}: {title}",
                            title = task_title(&self.db, &st)
                        ),
                    );
                }
                None => self.send(chat_id, &format!("No task matching #{short}")),
            },
            Command::Answer { id, text } => match self.find_task(&id) {
                Some(task) => match task.session_name.clone() {
                    Some(session) if self.tmux.window_exists(&session).unwrap_or(false) => {
                        self.inject_text(&session, &text);
                        self.send(chat_id, "✅ Sent.");
                    }
                    _ => self.send(chat_id, "That task has no running session."),
                },
                None => self.send(chat_id, &format!("No task matching #{id}")),
            },
            Command::Orchestrator(msg) => self.handle_orchestrator(chat_id, &msg),
            Command::Help => self.send(chat_id, &help_text()),
            Command::Unknown(c) => self.send(chat_id, &format!("Unknown command: /{c}\nTry /help")),
        }
    }

    fn handle_new(&mut self, chat_id: i64, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            self.send(chat_id, "Usage: /new <title>");
            return;
        }
        // Resolve agent + plugin + project_id from config / existing tasks.
        let global = crate::config::GlobalConfig::load().unwrap_or_default();
        let project = crate::config::ProjectConfig::load(&self.project_path).unwrap_or_default();
        let merged = crate::config::MergedConfig::merge(&global, &project);
        let existing = self.db.get_all_tasks().unwrap_or_default();
        let project_id = existing
            .first()
            .map(|t| t.project_id.clone())
            .unwrap_or_else(|| self.project_name.clone());

        let mut task = Task::new(title, merged.default_agent.clone(), project_id);
        task.plugin = merged.workflow_plugin.clone();
        let short = short_id(&task.id).to_string();
        match self.db.create_task(&task) {
            Ok(()) => {
                let kb = vec![vec![Button {
                    text: "▶️ Start".to_string(),
                    callback_data: serde_json::to_string(&CallbackData {
                        t: short.clone(),
                        k: "adv".to_string(),
                        p: String::new(),
                    })
                    .unwrap_or_default(),
                }]];
                let _ = self.api.send_message(
                    chat_id,
                    &format!(
                        "✅ Created #{short} \"{title}\" in backlog ({}).",
                        merged.default_agent
                    ),
                    None,
                    Some(kb),
                );
            }
            Err(e) => self.send(chat_id, &format!("Failed to create task: {e}")),
        }
    }

    // ── Transitions ─────────────────────────────────────────────────────────

    fn enqueue_advance(&mut self, chat_id: i64, task: &Task) {
        let Some(action) = next_action(task.status) else {
            self.send(
                chat_id,
                "This task can't be advanced from its current phase.",
            );
            return;
        };
        if task.status == TaskStatus::Backlog && !self.db.deps_satisfied(task) {
            self.send(
                chat_id,
                "Blocked: this task's dependencies aren't satisfied yet.",
            );
            return;
        }
        let verb = match action {
            "move_to_done" => "moving to done",
            _ => "advancing",
        };
        // After a task is marked done, offer to kickstart the next backlog task.
        let suggest_next = action == "move_to_done";
        self.enqueue_transition(chat_id, task, action, verb, suggest_next);
    }

    fn enqueue_transition(
        &mut self,
        chat_id: i64,
        task: &Task,
        action: &str,
        verb: &str,
        suggest_next: bool,
    ) {
        let req = TransitionRequest::new(task.id.clone(), action.to_string());
        let request_id = req.id.clone();
        let short = short_id(&task.id).to_string();
        let label = format!("{verb} #{short}");
        match self.db.create_transition_request(&req) {
            Ok(()) => {
                self.send(chat_id, &format!("⏳ {label} ({})…", task.status.as_str()));
                self.pending.push(PendingTransition {
                    request_id,
                    chat_id,
                    label,
                    deadline: Instant::now() + Duration::from_secs(90),
                    suggest_next,
                });
            }
            Err(e) => self.send(chat_id, &format!("Failed to queue {label}: {e}")),
        }
    }

    fn check_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut still = Vec::new();
        let drained: Vec<PendingTransition> = self.pending.drain(..).collect();
        for p in drained {
            match self.db.get_transition_request(&p.request_id) {
                Ok(Some(req)) if req.processed_at.is_some() => match req.error {
                    Some(err) => self.send(p.chat_id, &format!("❌ {}: {err}", p.label)),
                    None => {
                        self.send(p.chat_id, &format!("✅ {} — done.", p.label));
                        if p.suggest_next {
                            self.suggest_next_task(p.chat_id);
                        }
                    }
                },
                _ => {
                    if Instant::now() >= p.deadline {
                        self.send(
                            p.chat_id,
                            &format!(
                                "⏳ {} is still queued — is the agtx TUI running to process it?",
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

    /// Suggest the next dependency-satisfied backlog task with a one-tap Start button.
    fn suggest_next_task(&self, chat_id: i64) {
        let tasks = self.db.get_all_tasks().unwrap_or_default();
        let next = tasks
            .iter()
            .find(|t| t.status == TaskStatus::Backlog && self.db.deps_satisfied(t));
        match next {
            Some(t) => {
                let short = short_id(&t.id).to_string();
                let kb = vec![vec![Button {
                    text: "▶️ Start".to_string(),
                    callback_data: cb(&short, "adv", ""),
                }]];
                let _ = self.api.send_message(
                    chat_id,
                    &format!("Next up: #{short} {}", t.title),
                    None,
                    Some(kb),
                );
            }
            None => self.send(chat_id, "No backlog tasks are ready to start."),
        }
    }

    /// Delete Telegram question messages whose underlying pane has changed or whose
    /// window is gone — i.e. the question was already answered in the terminal (or the
    /// agent moved on), so the message is stale and shouldn't linger.
    fn prune_stale_routes(&mut self) {
        if self.routes.is_empty() {
            return;
        }
        let mut stale: Vec<(i64, i64)> = Vec::new(); // (message_id, chat_id)
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

    // ── Injection helpers ───────────────────────────────────────────────────

    fn inject_text(&self, session: &str, text: &str) {
        if text.contains('\n') {
            // Multi-line: paste then submit separately so the first newline doesn't submit early.
            let _ = self.tmux.paste_text(session, text);
            let _ = self.tmux.send_keys_literal(session, "Enter");
        } else {
            let _ = self.tmux.send_keys(session, text);
        }
    }

    fn inject_keys(&self, session: &str, kind: &str, payload: &str) {
        match kind {
            // text + Enter (digit menu choice, y/n)
            "send" => {
                let _ = self.tmux.send_keys(session, payload);
            }
            // named keys, no auto-Enter (e.g. "Down Enter" for arrow approvals)
            "keys" => {
                for key in payload.split_whitespace() {
                    let _ = self.tmux.send_keys_literal(session, key);
                }
            }
            _ => {}
        }
    }

    fn send_pane_excerpt(&self, chat_id: i64, task: &Task, reply_to: Option<i64>) {
        let Some(session) = &task.session_name else {
            self.send(chat_id, "That task has no session.");
            return;
        };
        let content = self.tmux.capture_pane(session).unwrap_or_default();
        let excerpt = extract::clean_pane(&content, 30, 3500);
        let text = format!("📋 #{}\n\n{}", short_id(&task.id), excerpt);
        let _ = self.api.send_message(chat_id, &text, reply_to, None);
    }

    /// View the orchestrator's conversation, or send it a message and report back shortly.
    fn handle_orchestrator(&mut self, chat_id: i64, msg: &str) {
        if !self.tmux.window_exists(&self.orch_target).unwrap_or(false) {
            self.send(
                chat_id,
                "Orchestrator isn't running. Toggle it with O in agtx (needs --experimental).",
            );
            return;
        }
        let msg = msg.trim();
        if msg.is_empty() {
            // No message — just show the conversation.
            self.send_orch_pane(chat_id);
            return;
        }
        // Send the message into the orchestrator pane, then capture its reply a few
        // seconds later (deferred so the bridge loop isn't blocked).
        let _ = self.tmux.send_keys(&self.orch_target, msg);
        self.send(chat_id, "📨 Sent to the orchestrator.");
        self.pending_orch_capture = Some((chat_id, Instant::now() + Duration::from_secs(4)));
    }

    /// Send the orchestrator's recent pane content (its conversation) to Telegram.
    fn send_orch_pane(&self, chat_id: i64) {
        let content = match self.tmux.capture_pane(&self.orch_target) {
            Ok(c) => c,
            Err(_) => {
                self.send(chat_id, "Couldn't read the orchestrator pane.");
                return;
            }
        };
        let body = extract::clean_pane(&content, 40, 3500);
        let text = if body.is_empty() {
            "🧭 Orchestrator (no visible output yet)".to_string()
        } else {
            format!("🧭 Orchestrator:\n\n{body}")
        };
        self.send(chat_id, &text);
    }
}

fn task_title(db: &Database, short: &str) -> String {
    db.get_all_tasks()
        .ok()
        .and_then(|all| {
            all.into_iter()
                .find(|t| t.id.starts_with(short) || short_id(&t.id) == short)
                .map(|t| t.title)
        })
        .unwrap_or_default()
}

fn format_question(check: &OutboundCheck, context: &str, kind: &QuestionKind) -> String {
    let mut text = format!(
        "🟡 #{} · {} · {}\n{}\n\n{}",
        short_id(&check.task_id),
        check.phase,
        check.agent,
        check.title,
        context
    );
    match kind {
        QuestionKind::FreeText => text.push_str("\n\n💬 Reply to this message to answer."),
        _ => text.push_str("\n\n👇 Tap an option below."),
    }
    text
}

fn build_keyboard(
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
                    callback_data: cb(short, "send", &opt.key),
                }]);
            }
        }
        QuestionKind::YesNo => {
            rows.push(vec![
                Button {
                    text: "✅ Yes".to_string(),
                    callback_data: cb(short, "send", "y"),
                },
                Button {
                    text: "❌ No".to_string(),
                    callback_data: cb(short, "send", "n"),
                },
            ]);
        }
        QuestionKind::FreeText => {}
    }
    rows.push(vec![Button {
        text: "📋 Show pane".to_string(),
        callback_data: cb(short, "pane", ""),
    }]);
    rows
}

fn cb(short: &str, k: &str, p: &str) -> String {
    serde_json::to_string(&CallbackData {
        t: short.to_string(),
        k: k.to_string(),
        p: p.to_string(),
    })
    .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn hash_pane(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_data_fits_telegram_limit() {
        let s = cb("a1b2c3d4", "send", "2");
        assert!(s.len() <= 64, "callback_data too long: {} bytes", s.len());
    }

    #[test]
    fn menu_keyboard_has_one_row_per_option_plus_pane() {
        let opts = vec![
            extract::ExtractedOption {
                key: "1".to_string(),
                label: "Yes".to_string(),
            },
            extract::ExtractedOption {
                key: "2".to_string(),
                label: "No".to_string(),
            },
        ];
        let kb = build_keyboard("a1b2c3d4", &QuestionKind::Menu, &opts);
        assert_eq!(kb.len(), 3); // 2 options + pane row
    }

    #[test]
    fn yesno_keyboard_has_two_buttons() {
        let kb = build_keyboard("a1b2c3d4", &QuestionKind::YesNo, &[]);
        assert_eq!(kb[0].len(), 2);
    }

    #[test]
    fn freetext_message_has_reply_hint() {
        let check = OutboundCheck {
            task_id: "a1b2c3d4ffff".to_string(),
            session_name: "s".to_string(),
            title: "T".to_string(),
            phase: "running".to_string(),
            agent: "claude".to_string(),
        };
        let text = format_question(&check, "Which format?", &QuestionKind::FreeText);
        assert!(text.contains("Reply to this message"));
        assert!(text.contains("Which format?"));
        assert!(text.contains("#a1b2c3d4"));
    }
}
