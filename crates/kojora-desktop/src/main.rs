//! Kojora Studio: bot-first native desktop user interface.
//! The headless kojora-studio crate owns storage and execution.

use eframe::egui;
use kojora_studio::{
    checks::{CheckKind, HookPhase},
    default_data_dir,
    worker::{StudioConnection, WorkerStatus},
    AgentMode, AgentProvider, StudioState, TaskRecord, TaskStatus,
};
use std::borrow::Cow;
use std::collections::{btree_map::Entry, BTreeMap};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

const COLOR_ACCENT: egui::Color32 = egui::Color32::from_rgb(114, 184, 255);
const COLOR_SUCCESS: egui::Color32 = egui::Color32::from_rgb(112, 215, 160);
const COLOR_ERROR: egui::Color32 = egui::Color32::from_rgb(250, 133, 130);
const WORKER_RETRY_DELAY: Duration = Duration::from_millis(1500);

struct WorkerStartup {
    socket: PathBuf,
    retry_at: Instant,
    attempt: Option<mpsc::Receiver<io::Result<StudioConnection>>>,
    error: Option<String>,
}

impl WorkerStartup {
    fn poll(&mut self, ctx: &egui::Context, now: Instant) -> Option<StudioConnection> {
        if let Some(attempt) = &self.attempt {
            let result = match attempt.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(io::Error::other("Worker connection thread stopped"))
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(WORKER_RETRY_DELAY);
                    return None;
                }
            };
            self.attempt = None;
            match result {
                Ok(studio) => return Some(studio),
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.retry_at = now + WORKER_RETRY_DELAY;
                }
            }
        }

        if now >= self.retry_at {
            let socket = self.socket.clone();
            let ctx = ctx.clone();
            let (sender, receiver) = mpsc::channel();
            let spawn = thread::Builder::new()
                .name("kojora-connect-worker".into())
                .spawn(move || {
                    let result = StudioConnection::connect_worker(socket);
                    let _ = sender.send(result);
                    ctx.request_repaint();
                });
            match spawn {
                Ok(_) => self.attempt = Some(receiver),
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.retry_at = now + WORKER_RETRY_DELAY;
                }
            }
        }
        ctx.request_repaint_after(if self.attempt.is_some() {
            WORKER_RETRY_DELAY
        } else {
            self.retry_at.saturating_duration_since(now)
        });
        None
    }
}

enum DiffPreview {
    Loading(mpsc::Receiver<Result<String, String>>),
    Ready(Result<String, String>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum InspectorOutput {
    #[default]
    Agent,
    Check(u64),
    Diff,
}

struct DesktopApp {
    studio: Option<Arc<StudioConnection>>,
    worker_startup: Option<WorkerStartup>,
    error: Option<String>,
    selected_bot: Option<u64>,
    selected_task: Option<u64>,
    inspector_output: InspectorOutput,
    history_page: usize,
    completed_responses: BTreeMap<u64, String>,
    adding_bot: bool,
    new_bot_name: String,
    new_workspace: String,
    new_provider: AgentProvider,
    new_profile: String,
    prompt: String,
    notice: String,
    show_stderr: bool,
    show_raw_events: bool,
    diff_preview: Option<(u64, DiffPreview)>,
    edit_mode: bool,
    confirm_edit: bool,
}

impl DesktopApp {
    fn new(target: LaunchTarget) -> Self {
        let (studio, worker_startup, error) = match target {
            LaunchTarget::Worker(socket) => (
                None,
                Some(WorkerStartup {
                    socket,
                    retry_at: Instant::now(),
                    attempt: None,
                    error: None,
                }),
                None,
            ),
            LaunchTarget::Local(root) => match StudioConnection::open_local(root) {
                Ok(studio) => (Some(Arc::new(studio)), None, None),
                Err(error) => (None, None, Some(error.to_string())),
            },
        };
        Self {
            studio,
            worker_startup,
            error,
            selected_bot: None,
            selected_task: None,
            inspector_output: InspectorOutput::default(),
            history_page: 0,
            completed_responses: BTreeMap::new(),
            adding_bot: false,
            new_bot_name: String::new(),
            new_workspace: std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            new_provider: AgentProvider::default(),
            new_profile: String::new(),
            prompt: String::new(),
            notice: String::new(),
            show_stderr: false,
            show_raw_events: false,
            diff_preview: None,
            edit_mode: false,
            confirm_edit: false,
        }
    }

    fn select_task(&mut self, task: Option<u64>) {
        if self.selected_task != task {
            self.selected_task = task;
            self.inspector_output = InspectorOutput::Agent;
            self.show_stderr = false;
            self.show_raw_events = false;
        }
    }

    fn create_bot(&mut self) {
        let Some(studio) = &self.studio else {
            return;
        };
        match studio.add_bot_with_provider(
            &self.new_bot_name,
            PathBuf::from(&self.new_workspace),
            PathBuf::from(&self.new_profile),
            self.new_provider,
        ) {
            Ok(id) => {
                self.selected_bot = Some(id);
                self.select_task(None);
                self.history_page = 0;
                self.adding_bot = false;
                self.new_bot_name.clear();
                self.new_profile.clear();
                self.edit_mode = false;
                self.notice = "Bot created.".into();
            }
            Err(error) => self.notice = format!("Could not create bot: {error}"),
        }
    }

    fn send_prompt(&mut self) {
        let Some(bot_id) = self.selected_bot else {
            self.notice = "Select or create a bot first.".into();
            return;
        };
        let Some(studio) = &self.studio else {
            return;
        };
        let mode = if self.edit_mode {
            AgentMode::EditWorktree
        } else {
            AgentMode::Inspect
        };
        match studio.submit_with_mode(bot_id, &self.prompt, mode) {
            Ok(task_id) => {
                self.select_task(Some(task_id));
                self.notice = format!("Task #{task_id} started.");
                self.prompt.clear();
            }
            Err(error) => {
                self.notice = format!("Could not launch agent: {error}");
            }
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, state: &StudioState) {
        ui.add_space(12.0);
        ui.heading(egui::RichText::new("KOJORA").color(COLOR_ACCENT).size(22.0));
        ui.label(
            egui::RichText::new("Software Factory / Local MVP")
                .weak()
                .small(),
        );
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.strong("Bots");
            if ui.small_button("+ New").clicked() {
                self.adding_bot = !self.adding_bot;
            }
        });
        ui.separator();

        let reserved_footer_height = 68.0;
        let list_height = (ui.available_height() - reserved_footer_height).max(100.0);
        egui::ScrollArea::vertical()
            .id_salt("bot-list")
            .max_height(list_height)
            .show(ui, |ui| {
                for bot in &state.bots {
                    let selected = self.selected_bot == Some(bot.id);
                    let label = format!("◉  {}", bot.name);
                    if ui.selectable_label(selected, label).clicked() {
                        self.selected_bot = Some(bot.id);
                        self.select_task(None);
                        self.history_page = 0;
                        self.edit_mode = false;
                    }
                    ui.label(
                        egui::RichText::new(format!("     {} · #{}", bot.provider.label(), bot.id))
                            .weak()
                            .small(),
                    );
                    ui.add_space(7.0);
                }
                if self.adding_bot {
                    ui.separator();
                    ui.strong("New bot");
                    ui.label("Name");
                    ui.text_edit_singleline(&mut self.new_bot_name);
                    ui.label("Workspace (absolute path)");
                    ui.text_edit_singleline(&mut self.new_workspace);
                    egui::ComboBox::from_id_salt("new-bot-provider")
                        .selected_text(self.new_provider.label())
                        .show_ui(ui, |ui| {
                            for provider in AgentProvider::ALL {
                                ui.selectable_value(
                                    &mut self.new_provider,
                                    provider,
                                    provider.label(),
                                );
                            }
                        });
                    ui.label("Account profile (absolute path)");
                    ui.text_edit_singleline(&mut self.new_profile);
                    ui.label(
                        egui::RichText::new(
                            "Use an existing 0700 private profile, outside the workspace.",
                        )
                        .small()
                        .weak(),
                    );
                    if ui.button("Create bot").clicked() {
                        self.create_bot();
                    }
                }
            });

        ui.separator();
        if let Some(studio) = &self.studio {
            ui.label(
                egui::RichText::new(studio.data_dir().to_string_lossy().to_string())
                    .small()
                    .weak(),
            );
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, state: &StudioState, status: &WorkerStatus) {
        ui.horizontal(|ui| {
            ui.heading("Studio");
            ui.separator();
            let label = self
                .selected_bot
                .and_then(|id| state.bots.iter().find(|bot| bot.id == id))
                .map_or("Select a Bot", |bot| bot.name.as_str());
            ui.label(label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let stop_target = status
                    .active_tasks
                    .iter()
                    .find(|task| Some(task.task_id) == self.selected_task)
                    .or_else(|| status.active_tasks.first());
                if let (Some(studio), Some(task)) = (&self.studio, stop_target) {
                    let task_id = task.task_id;
                    if task.cancellation_requested {
                        ui.colored_label(COLOR_ERROR, format!("Stopping task #{task_id}…"));
                    } else if ui.button(format!("Stop task #{task_id}")).clicked() {
                        self.notice = match studio.cancel_task(task_id) {
                            Ok(()) => format!("Cancellation requested for task #{task_id}."),
                            Err(error) => format!("Could not stop task: {error}"),
                        };
                    }
                }
                if status.active_count > 0 {
                    ui.spinner();
                    ui.colored_label(
                        COLOR_ACCENT,
                        format!(
                            "Active tasks {}/{}",
                            status.active_count, status.concurrency
                        ),
                    );
                } else if status.checks_pending > 0 {
                    ui.spinner();
                    ui.label("Local checks pending");
                } else {
                    ui.colored_label(COLOR_SUCCESS, "Ready");
                }
            });
        });
        if !self.notice.is_empty() {
            ui.label(egui::RichText::new(&self.notice).small().weak());
        }
        if let Some(error) = &status.last_error {
            ui.colored_label(COLOR_ERROR, error);
        }
    }

    fn conversation(&mut self, ui: &mut egui::Ui, state: &StudioState, status: &WorkerStatus) {
        let selected = self.selected_bot;
        let Some(bot_id) = selected else {
            ui.vertical_centered(|ui| {
                ui.add_space(95.0);
                ui.heading("Your software factory starts with a Bot");
                ui.label("Create a Bot, select a provider and account profile, then send a task.");
            });
            return;
        };

        let Some(bot) = state.bots.iter().find(|bot| bot.id == bot_id) else {
            return;
        };
        ui.small(format!("Workspace: {}", bot.workspace.display()));
        ui.small(format!("Account profile: {}", bot.profile_dir.display()));
        ui.separator();

        let tasks: Vec<&TaskRecord> = state
            .tasks
            .iter()
            .filter(|task| task.bot_id == bot_id)
            .collect();

        let max_page = tasks.len().saturating_sub(1) / 60;
        self.history_page = self.history_page.min(max_page);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.history_page > 0, egui::Button::new("Newer"))
                .clicked()
            {
                self.history_page -= 1;
            }
            ui.label(format!(
                "History page {} / {}",
                self.history_page + 1,
                max_page + 1
            ));
            if ui
                .add_enabled(self.history_page < max_page, egui::Button::new("Older"))
                .clicked()
            {
                self.history_page += 1;
            }
        });
        let end = tasks.len().saturating_sub(self.history_page * 60);
        let page = &tasks[end.saturating_sub(60)..end];
        self.completed_responses
            .retain(|id, _| page.iter().any(|task| task.id == *id));

        egui::ScrollArea::vertical()
            .id_salt(("conversation", self.history_page))
            .auto_shrink([false, false])
            .stick_to_bottom(self.history_page == 0)
            .show(ui, |ui| {
                if tasks.is_empty() {
                    ui.add_space(60.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("No tasks yet");
                        ui.label("Describe what you want the agent to inspect.");
                    });
                }
                for task in page {
                    let selected = self.selected_task == Some(task.id);
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(selected, format!("Task #{}", task.id))
                                .clicked()
                            {
                                self.select_task(Some(task.id));
                            }
                            status_label(ui, task.status);
                        });
                        ui.label(&task.prompt);
                        if task.mode == AgentMode::EditWorktree {
                            ui.label(
                                egui::RichText::new("Isolated edit workspace")
                                    .color(COLOR_ACCENT)
                                    .small(),
                            );
                        }
                        let response = self.conversation_response(task, bot.provider);
                        if !response.is_empty() {
                            ui.separator();
                            ui.label(
                                egui::RichText::new(bot.provider.label())
                                    .color(COLOR_ACCENT)
                                    .strong(),
                            );
                            ui.label(response);
                        } else if task.status == TaskStatus::Running {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                let stopping = status.active_tasks.iter().any(|active| {
                                    active.task_id == task.id && active.cancellation_requested
                                });
                                ui.label(if stopping {
                                    "Stopping the agent and collecting logs..."
                                } else {
                                    "The agent is processing this task..."
                                });
                            });
                        } else {
                            ui.label(
                                egui::RichText::new("Select this task to inspect logs and status.")
                                    .weak()
                                    .small(),
                            );
                        }
                    });
                    ui.add_space(10.0);
                }
            });
    }

    fn conversation_response(
        &mut self,
        task: &TaskRecord,
        provider: AgentProvider,
    ) -> Cow<'_, str> {
        let read = || {
            self.studio
                .as_ref()?
                .read_log_tail(task.id, false)
                .ok()
                .map(|raw| provider.extract_text(&raw))
        };
        if matches!(
            task.status,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return match self.completed_responses.entry(task.id) {
                Entry::Occupied(response) => Cow::Borrowed(response.into_mut()),
                Entry::Vacant(slot) => match read() {
                    Some(response) => Cow::Borrowed(slot.insert(response)),
                    None => Cow::Borrowed(""),
                },
            };
        }
        Cow::Owned(read().unwrap_or_default())
    }

    fn inspector(&mut self, ui: &mut egui::Ui, state: &StudioState, status: &WorkerStatus) {
        ui.add_space(12.0);
        ui.heading("Run inspector");
        ui.separator();
        let Some(task_id) = self.selected_task else {
            ui.label("Select a task to inspect its Run, Session and log output.");
            return;
        };
        let Some(task) = state.tasks.iter().find(|task| task.id == task_id) else {
            self.select_task(None);
            return;
        };
        ui.strong(format!("Task #{}", task.id));
        status_label(ui, task.status);
        if let Some(run) = state.runs.iter().find(|run| run.id == task.run_id) {
            ui.add_space(8.0);
            egui::Grid::new("run_metadata")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Bot");
                    ui.label(format!("#{}", task.bot_id));
                    ui.end_row();
                    ui.label("Run");
                    ui.label(format!("#{}", run.id));
                    ui.end_row();
                    ui.label("Session");
                    ui.label(format!("#{}", run.session_id));
                    ui.end_row();
                    ui.label("Exit code");
                    ui.label(
                        run.exit_code
                            .map_or_else(|| "—".to_owned(), |exit| exit.to_string()),
                    );
                    ui.end_row();
                    ui.label("Status");
                    ui.label(run.status.label());
                    ui.end_row();
                });
            if let Some(error) = &run.error {
                ui.add_space(6.0);
                ui.colored_label(COLOR_ERROR, error);
            }
            if let Some(worktree) = &run.worktree_dir {
                ui.separator();
                ui.label("Isolated Git worktree");
                ui.small(worktree.to_string_lossy().to_string());
                if ui.small_button("Copy worktree path").clicked() {
                    ui.ctx().copy_text(worktree.to_string_lossy().to_string());
                }
            }
            if run.worktree_dir.is_some() && task.status != TaskStatus::Running {
                if let Some(studio) = &self.studio {
                    if status.checks_enabled {
                        ui.separator();
                        ui.label("Central checks (trusted worktrees only)");
                        ui.horizontal_wrapped(|ui| {
                            for (name, kind) in [
                                ("Queue fmt", CheckKind::RustFmt),
                                ("Queue clippy", CheckKind::RustClippy),
                                ("Queue tests", CheckKind::RustTest),
                            ] {
                                if ui.small_button(name).clicked() {
                                    self.notice = match studio.queue_check(
                                        task_id,
                                        HookPhase::Manual,
                                        kind,
                                    ) {
                                        Ok(id) => format!("Check #{id} queued by Worker"),
                                        Err(error) => format!("Cannot queue check: {error}"),
                                    };
                                }
                            }
                        });
                    }
                    if let Ok(checks) = studio.checks() {
                        for check in checks.iter().rev().filter(|r| r.task_id == task_id).take(5) {
                            ui.selectable_value(
                                &mut self.inspector_output,
                                InspectorOutput::Check(check.id),
                                format!("#{} {:?}: {:?}", check.id, check.kind, check.status),
                            );
                        }
                        if let InspectorOutput::Check(check_id) = self.inspector_output {
                            if !checks
                                .iter()
                                .any(|r| r.id == check_id && r.task_id == task_id)
                            {
                                self.inspector_output = InspectorOutput::Agent;
                            }
                        }
                    }
                }
            }
            if let Some(dir) = &run.run_dir {
                ui.separator();
                ui.label("Run directory");
                ui.small(dir.to_string_lossy().to_string());
                if ui.small_button("Copy path").clicked() {
                    ui.ctx().copy_text(dir.to_string_lossy().to_string());
                }
            }
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.inspector_output,
                InspectorOutput::Agent,
                "Agent log",
            );
            if task.mode == AgentMode::EditWorktree {
                ui.selectable_value(
                    &mut self.inspector_output,
                    InspectorOutput::Diff,
                    "Worktree diff",
                );
            }
        });

        if self.inspector_output == InspectorOutput::Diff {
            ui.separator();
            if task.status == TaskStatus::Running {
                ui.label("The worktree diff is available after the task finishes.");
            } else {
                self.worktree_diff(ui, task_id);
            }
            return;
        }

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.show_stderr, false, "Stdout");
            ui.selectable_value(&mut self.show_stderr, true, "Stderr");
            if self.inspector_output == InspectorOutput::Agent && !self.show_stderr {
                ui.checkbox(&mut self.show_raw_events, "Raw JSONL");
            }
        });
        ui.separator();

        if let Some(studio) = &self.studio {
            let output_result = if let InspectorOutput::Check(check_id) = self.inspector_output {
                studio.check_log(check_id, self.show_stderr)
            } else {
                studio.read_log_tail(task_id, self.show_stderr)
            };
            match output_result {
                Ok(raw) => {
                    let output = if self.inspector_output == InspectorOutput::Agent
                        && !self.show_stderr
                        && !self.show_raw_events
                    {
                        let provider = state
                            .bots
                            .iter()
                            .find(|bot| bot.id == task.bot_id)
                            .map_or(AgentProvider::default(), |bot| bot.provider);
                        let text = provider.extract_text(&raw);
                        if text.is_empty() {
                            raw
                        } else {
                            text
                        }
                    } else {
                        raw
                    };
                    egui::ScrollArea::vertical()
                        .id_salt("inspector")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if output.is_empty() {
                                ui.label(egui::RichText::new("No output yet").weak());
                            } else {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(output).monospace())
                                        .wrap(),
                                );
                            }
                        });
                }
                Err(error) => {
                    ui.colored_label(COLOR_ERROR, error.to_string());
                }
            }
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Only the latest 32 KiB is loaded. Logs may contain sensitive model output.",
            )
            .small()
            .weak(),
        );
    }

    fn worktree_diff(&mut self, ui: &mut egui::Ui, task_id: u64) {
        if let Some((id, DiffPreview::Loading(receiver))) = &self.diff_preview {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err("Worktree diff worker stopped.".into())
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ui.spinner();
                    return;
                }
            };
            self.diff_preview = Some((*id, DiffPreview::Ready(result)));
        }
        if let Some((id, DiffPreview::Ready(result))) = &self.diff_preview {
            if *id == task_id && !ui.small_button("Refresh diff").clicked() {
                match result {
                    Ok(output) => {
                        egui::ScrollArea::vertical()
                            .id_salt("worktree_diff")
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(output).monospace())
                                        .wrap(),
                                );
                            });
                    }
                    Err(error) => {
                        ui.colored_label(COLOR_ERROR, error);
                    }
                }
                return;
            }
        }
        let Some(studio) = self.studio.clone() else {
            return;
        };
        let ctx = ui.ctx().clone();
        let (sender, receiver) = mpsc::channel();
        let spawn = thread::Builder::new()
            .name("kojora-worktree-diff".into())
            .spawn(move || {
                let result = studio
                    .read_worktree_diff(task_id)
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
                ctx.request_repaint();
            });
        self.diff_preview = Some((
            task_id,
            match spawn {
                Ok(_) => DiffPreview::Loading(receiver),
                Err(error) => DiffPreview::Ready(Err(error.to_string())),
            },
        ));
        ui.spinner();
        ui.ctx().request_repaint();
    }

    fn composer(&mut self, ui: &mut egui::Ui, state: &StudioState, status: &WorkerStatus) {
        let provider = state
            .bots
            .iter()
            .find(|bot| Some(bot.id) == self.selected_bot)
            .map_or(AgentProvider::default(), |bot| bot.provider);
        let supports_edits = provider.edit_adapter().is_some();
        if !supports_edits {
            self.edit_mode = false;
        }
        let slots_available = status.active_count < status.concurrency;
        ui.horizontal(|ui| {
            ui.colored_label(COLOR_ACCENT, provider.label());
            ui.label(
                egui::RichText::new(if self.edit_mode {
                    "Isolated worktree · workspace-write · opt-in"
                } else {
                    "Read-only · non-interactive · local"
                })
                .weak()
                .small(),
            );
        });
        if supports_edits {
            ui.checkbox(
                &mut self.edit_mode,
                "Allow edits in a separate Git worktree",
            );
        }
        if self.edit_mode {
            ui.colored_label(
                COLOR_ERROR,
                "Requires a trusted Git repository. Changes stay in Studio's worktree until reviewed.",
            );
        }
        let edit = egui::TextEdit::multiline(&mut self.prompt)
            .hint_text("Ask the agent to inspect your project or explain an issue…")
            .desired_rows(3);
        ui.add_enabled(self.selected_bot.is_some(), edit);
        ui.horizontal(|ui| {
            let ready = !self.confirm_edit
                && slots_available
                && self.selected_bot.is_some()
                && !self.prompt.trim().is_empty();
            let shortcut =
                ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::Enter));
            if (ui
                .add_enabled(ready, egui::Button::new("Send task"))
                .clicked()
                || shortcut && ready)
                && ready
            {
                if self.edit_mode {
                    self.confirm_edit = true;
                } else {
                    self.send_prompt();
                }
            }
            ui.label(egui::RichText::new("Cmd/Ctrl+Enter to send").small().weak());
            if !slots_available {
                ui.label("Task slots full");
            }
        });
    }
}

impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(startup) = &mut self.worker_startup {
            if let Some(studio) = startup.poll(ctx, Instant::now()) {
                self.studio = Some(Arc::new(studio));
                self.worker_startup = None;
            } else {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.heading("Waiting for Kojora Worker");
                    ui.label(startup.socket.display().to_string());
                    if let Some(error) = &startup.error {
                        ui.colored_label(COLOR_ERROR, error);
                    }
                    ui.label("Start the Worker to connect automatically.");
                });
                return;
            }
        }

        if let Some(error) = &self.error {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Kojora Studio could not open");
                ui.colored_label(COLOR_ERROR, error);
                ui.label("Check that the data directory is private and not locked by another Studio instance.");
            });
            return;
        }

        let Some(studio) = &self.studio else {
            return;
        };
        let (status, state) = match studio
            .status()
            .and_then(|status| Ok((status, studio.snapshot()?)))
        {
            Ok(update) => update,
            Err(error) => {
                ctx.request_repaint_after(Duration::from_millis(1500));
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.heading("Studio connection unavailable");
                    ui.colored_label(COLOR_ERROR, error.to_string());
                    ui.label("Retrying connection…");
                });
                return;
            }
        };
        ctx.request_repaint_after(Duration::from_millis(
            if status.active_count > 0 || status.checks_pending > 0 {
                350
            } else {
                1500
            },
        ));
        if self.selected_bot.is_none() {
            self.selected_bot = state.bots.first().map(|bot| bot.id);
        }

        egui::TopBottomPanel::top("top_bar")
            .min_height(68.0)
            .show(ctx, |ui| {
                ui.add_space(7.0);
                self.header(ui, &state, &status);
            });

        egui::SidePanel::left("bots")
            .resizable(true)
            .default_width(240.0)
            .min_width(190.0)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!self.confirm_edit, |ui| self.sidebar(ui, &state));
            });

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(330.0)
            .min_width(260.0)
            .show(ctx, |ui| self.inspector(ui, &state, &status));

        egui::TopBottomPanel::bottom("composer")
            .resizable(false)
            .min_height(130.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.add_enabled_ui(!self.confirm_edit, |ui| self.composer(ui, &state, &status));
                ui.add_space(7.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| self.conversation(ui, &state, &status));

        if self.confirm_edit {
            let modal = egui::Modal::new(egui::Id::new("confirm-isolated-edit")).show(ctx, |ui| {
                ui.heading("Confirm isolated edit run");
                ui.label("Codex may change files inside a separate detached Git worktree.");
                ui.label("This must be a trusted project. No automatic commit, push or merge.");
                ui.colored_label(
                    COLOR_ERROR,
                    "The workspace-write sandbox is not a complete security boundary.",
                );
                ui.horizontal(|ui| {
                    let approved = ui.button("Confirm and run").clicked();
                    let cancelled = ui.button("Cancel").clicked();
                    (approved, cancelled)
                })
                .inner
            });
            let (approved, cancelled) = modal.inner;
            self.confirm_edit = !(approved || cancelled || modal.should_close());
            if approved {
                self.send_prompt();
            }
        }
    }
}

/// Use an OS-installed CJK font when available; no font files are shipped.
fn install_cjk_fallback(ctx: &egui::Context) {
    let mut candidates = Vec::new();
    if let Some(custom) = std::env::var_os("KOJORA_GUI_FONT") {
        candidates.push(PathBuf::from(custom));
    }
    if cfg!(target_os = "macos") {
        candidates.extend([
            PathBuf::from("/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc"),
            PathBuf::from("/System/Library/Fonts/Hiragino Sans GB.ttc"),
            PathBuf::from("/Library/Fonts/Arial Unicode.ttf"),
        ]);
    }
    if cfg!(target_os = "linux") {
        candidates.extend([
            PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansJP-Regular.otf"),
        ]);
    }
    let font = candidates.into_iter().find_map(|path| {
        let metadata = std::fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > 40 * 1024 * 1024 {
            return None;
        }
        std::fs::read(path).ok()
    });
    if let Some(font) = font {
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("kojora-cjk".into(), egui::FontData::from_owned(font).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("kojora-cjk".to_owned());
        }
        ctx.set_fonts(fonts);
    }
}

fn status_label(ui: &mut egui::Ui, status: TaskStatus) {
    let color = match status {
        TaskStatus::Succeeded => COLOR_SUCCESS,
        TaskStatus::Running => COLOR_ACCENT,
        TaskStatus::Cancelled => egui::Color32::GRAY,
        TaskStatus::Failed | TaskStatus::Interrupted => COLOR_ERROR,
    };
    ui.colored_label(color, status.label());
}

const USAGE: &str =
    "Usage: kojora-desktop [--data-dir ABSOLUTE_DIR] [--worker-socket ABSOLUTE_SOCKET]";

#[derive(Debug, PartialEq, Eq)]
enum LaunchTarget {
    Local(PathBuf),
    Worker(PathBuf),
}

fn launch_target(
    mut args: impl Iterator<Item = OsString>,
    default_path: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<Option<LaunchTarget>> {
    let mut data_dir = None;
    let mut worker_socket = None;
    while let Some(arg) = args.next() {
        let destination = match arg.to_string_lossy().as_ref() {
            "--data-dir" => &mut data_dir,
            "--worker-socket" => &mut worker_socket,
            "-h" | "--help" => return Ok(None),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {arg:?}; run kojora-desktop --help"),
                ))
            }
        };
        let path = args.next().map(PathBuf::from).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{arg:?} requires an absolute path"),
            )
        })?;
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{arg:?} requires an absolute path"),
            ));
        }
        *destination = Some(path);
    }
    let target = match worker_socket {
        Some(socket) => LaunchTarget::Worker(socket),
        None => LaunchTarget::Local(data_dir.map_or_else(default_path, Ok)?),
    };
    Ok(Some(target))
}

fn main() -> ExitCode {
    let target = match launch_target(std::env::args_os().skip(1), default_data_dir) {
        Ok(Some(target)) => target,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("kojora-desktop: {error}");
            return ExitCode::FAILURE;
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1250.0, 800.0])
            .with_min_inner_size([940.0, 650.0]),
        ..Default::default()
    };
    match eframe::run_native(
        "Kojora Studio",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            install_cjk_fallback(&cc.egui_ctx);
            Ok(Box::new(DesktopApp::new(target)))
        }),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("kojora-desktop: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn desktop_can_connect_after_starting_before_the_worker() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!("kojora-late-worker-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let socket = root.join("worker.sock");
        let app = DesktopApp::new(LaunchTarget::Worker(socket));
        let mut startup = app
            .worker_startup
            .ok_or_else(|| io::Error::other("Worker startup is missing"))?;
        let ctx = egui::Context::default();

        for _ in 0..200 {
            assert!(startup.poll(&ctx, Instant::now()).is_none());
            if startup.error.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(startup.error.is_some(), "initial connection must fail");

        let worker = kojora_studio::Studio::open(&root)?;
        let server = thread::spawn(|| kojora_studio::worker::serve(worker, Default::default()));
        let mut connected = None;
        for _ in 0..500 {
            connected = startup.poll(&ctx, Instant::now());
            if connected.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let Some(StudioConnection::Worker(client)) = connected else {
            return Err(io::Error::other(
                "Desktop did not connect after Worker startup",
            ));
        };
        assert!(client.snapshot()?.bots.is_empty());
        client.shutdown()?;
        server
            .join()
            .map_err(|_| io::Error::other("Worker thread panicked"))??;
        fs::remove_dir_all(root)
    }

    #[test]
    fn switching_tasks_resets_the_inspector_to_agent_stdout() {
        let mut app = DesktopApp::new(LaunchTarget::Local(PathBuf::new()));
        app.select_task(Some(1));
        app.inspector_output = InspectorOutput::Check(7);
        app.show_stderr = true;
        app.show_raw_events = true;

        app.select_task(Some(1));
        assert_eq!(app.inspector_output, InspectorOutput::Check(7));

        app.select_task(Some(2));
        assert_eq!(app.selected_task, Some(2));
        assert_eq!(app.inspector_output, InspectorOutput::Agent);
        assert!(!app.show_stderr);
        assert!(!app.show_raw_events);
    }

    #[test]
    fn explicit_directory_and_socket_do_not_require_a_default() {
        let unavailable = || Err(io::Error::new(io::ErrorKind::NotFound, "HOME is unset"));
        for (flag, target) in [
            ("--data-dir", LaunchTarget::Local("/tmp/kojora".into())),
            (
                "--worker-socket",
                LaunchTarget::Worker("/tmp/kojora".into()),
            ),
        ] {
            let result = launch_target(
                [flag, "/tmp/kojora"].into_iter().map(OsString::from),
                unavailable,
            );
            assert!(matches!(result, Ok(Some(actual)) if actual == target));
        }
        assert!(launch_target(std::iter::empty(), unavailable).is_err());
    }
}
