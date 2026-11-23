//! Kojora Studio: bot-first native desktop user interface.
//! The headless kojora-studio crate owns storage and execution.

use eframe::egui;
use kojora_studio::{
    checks::{CheckKind, HookPhase},
    default_data_dir, extract_agent_text,
    worker::StudioConnection,
    AgentMode, StudioState, TaskRecord, TaskStatus,
};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

const COLOR_ACCENT: egui::Color32 = egui::Color32::from_rgb(114, 184, 255);
const COLOR_SUCCESS: egui::Color32 = egui::Color32::from_rgb(112, 215, 160);
const COLOR_ERROR: egui::Color32 = egui::Color32::from_rgb(250, 133, 130);

enum DiffPreview {
    Loading(mpsc::Receiver<Result<String, String>>),
    Ready(Result<String, String>),
}

struct DesktopApp {
    studio: Option<Arc<StudioConnection>>,
    error: Option<String>,
    selected_bot: Option<u64>,
    selected_task: Option<u64>,
    selected_check: Option<u64>,
    history_page: usize,
    adding_bot: bool,
    new_bot_name: String,
    new_workspace: String,
    new_profile: String,
    prompt: String,
    notice: String,
    show_stderr: bool,
    show_raw_events: bool,
    show_diff: bool,
    diff_preview: Option<(u64, DiffPreview)>,
    edit_mode: bool,
    confirm_edit: bool,
}

impl DesktopApp {
    fn new(root: PathBuf, socket: Option<PathBuf>) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let result = match socket {
            Some(socket) => StudioConnection::connect_worker(socket),
            None => StudioConnection::open_local(root),
        };
        let (studio, error) = match result {
            Ok(studio) => (Some(Arc::new(studio)), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            studio,
            error,
            selected_bot: None,
            selected_task: None,
            selected_check: None,
            history_page: 0,
            adding_bot: false,
            new_bot_name: String::new(),
            new_workspace: std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            new_profile: home
                .join(".config")
                .join("kojora")
                .join("profiles")
                .join("codex-primary")
                .to_string_lossy()
                .into_owned(),
            prompt: String::new(),
            notice: String::new(),
            show_stderr: false,
            show_raw_events: false,
            show_diff: false,
            diff_preview: None,
            edit_mode: false,
            confirm_edit: false,
        }
    }

    fn create_bot(&mut self) {
        let Some(studio) = &self.studio else {
            return;
        };
        match studio.add_bot(
            &self.new_bot_name,
            PathBuf::from(&self.new_workspace),
            PathBuf::from(&self.new_profile),
        ) {
            Ok(id) => {
                self.selected_bot = Some(id);
                self.selected_task = None;
                self.history_page = 0;
                self.adding_bot = false;
                self.new_bot_name.clear();
                self.notice =
                    "Bot created. Configure an authenticated Codex profile before running.".into();
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
                self.selected_task = Some(task_id);
                self.notice = format!("Task #{task_id} started.");
                self.prompt.clear();
                self.show_stderr = false;
                self.show_raw_events = false;
                self.show_diff = false;
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
                        self.selected_task = None;
                        self.history_page = 0;
                        self.show_stderr = false;
                        self.show_raw_events = false;
                    }
                    ui.label(
                        egui::RichText::new(format!("     Codex · #{}", bot.id))
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
                    ui.label("Codex profile (absolute path)");
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
        ui.label(
            egui::RichText::new("Read-only agent · 1 active run")
                .small()
                .weak(),
        );
        if let Some(studio) = &self.studio {
            ui.label(
                egui::RichText::new(studio.data_dir().to_string_lossy().to_string())
                    .small()
                    .weak(),
            );
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, state: &StudioState) {
        ui.horizontal(|ui| {
            ui.heading("Studio");
            ui.separator();
            let label = self
                .selected_bot
                .and_then(|id| state.bots.iter().find(|bot| bot.id == id))
                .map_or("Select a Bot", |bot| bot.name.as_str());
            ui.label(label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let active_task = self
                    .studio
                    .as_deref()
                    .and_then(StudioConnection::active_task_id);
                if let (Some(studio), Some(task_id)) = (&self.studio, active_task) {
                    if studio.cancellation_requested(task_id) {
                        ui.spinner();
                        ui.colored_label(COLOR_ERROR, format!("Stopping task #{task_id}..."));
                    } else {
                        if ui.button(format!("Stop task #{task_id}")).clicked() {
                            self.notice = match studio.cancel_task(task_id) {
                                Ok(()) => format!("Cancellation requested for task #{task_id}."),
                                Err(error) => format!("Could not stop task: {error}"),
                            };
                        }
                        ui.spinner();
                        ui.colored_label(COLOR_ACCENT, "Agent running");
                    }
                } else {
                    ui.colored_label(COLOR_SUCCESS, "Ready");
                }
            });
        });
        if !self.notice.is_empty() {
            ui.label(egui::RichText::new(&self.notice).small().weak());
        }
        if let Some(studio) = &self.studio {
            if let Some(error) = studio.last_error() {
                ui.colored_label(COLOR_ERROR, error);
            }
        }
    }

    fn conversation(&mut self, ui: &mut egui::Ui, state: &StudioState) {
        let selected = self.selected_bot;
        let Some(bot_id) = selected else {
            ui.vertical_centered(|ui| {
                ui.add_space(95.0);
                ui.heading("Your software factory starts with a Bot");
                ui.label("Create a Bot, select a workspace and Codex profile, then send a task.");
            });
            return;
        };

        let bot = state.bots.iter().find(|bot| bot.id == bot_id);
        if let Some(bot) = bot {
            ui.label(
                egui::RichText::new(format!("Workspace: {}", bot.workspace.to_string_lossy()))
                    .small()
                    .weak(),
            );
        }
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
        let start = end.saturating_sub(60);

        egui::ScrollArea::vertical()
            .id_salt(("conversation", self.history_page))
            .auto_shrink([false, false])
            .stick_to_bottom(self.history_page == 0)
            .show(ui, |ui| {
                if tasks.is_empty() {
                    ui.add_space(60.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("No tasks yet");
                        ui.label("Describe what you want Codex to inspect.");
                    });
                }
                for task in &tasks[start..end] {
                    let selected = self.selected_task == Some(task.id);
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(selected, format!("Task #{}", task.id))
                                .clicked()
                            {
                                self.selected_task = Some(task.id);
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
                        let response = self
                            .studio
                            .as_ref()
                            .and_then(|studio| studio.read_log_tail(task.id, false).ok())
                            .map(|raw| extract_agent_text(&raw))
                            .unwrap_or_default();
                        if !response.is_empty() {
                            ui.separator();
                            ui.label(egui::RichText::new("Codex").color(COLOR_ACCENT).strong());
                            ui.label(response);
                        } else if task.status == TaskStatus::Running {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                let stopping = self
                                    .studio
                                    .as_ref()
                                    .is_some_and(|studio| studio.cancellation_requested(task.id));
                                ui.label(if stopping {
                                    "Stopping Codex and collecting logs..."
                                } else {
                                    "Codex is processing this task..."
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

    fn inspector(&mut self, ui: &mut egui::Ui, state: &StudioState) {
        ui.add_space(12.0);
        ui.heading("Run inspector");
        ui.separator();
        let Some(task_id) = self.selected_task else {
            ui.label("Select a task to inspect its Run, Session and log output.");
            return;
        };
        let Some(task) = state.tasks.iter().find(|task| task.id == task_id) else {
            self.selected_task = None;
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
                    ui.separator();
                    ui.label("Central checks (trusted worktrees only)");
                    ui.horizontal_wrapped(|ui| {
                        for (name, kind) in [
                            ("Queue fmt", CheckKind::RustFmt),
                            ("Queue clippy", CheckKind::RustClippy),
                            ("Queue tests", CheckKind::RustTest),
                        ] {
                            if ui.small_button(name).clicked() {
                                self.notice =
                                    match studio.queue_check(task_id, HookPhase::Manual, kind) {
                                        Ok(id) => format!("Check #{id} queued by Worker"),
                                        Err(error) => format!("Cannot queue check: {error}"),
                                    };
                            }
                        }
                    });
                    if let Ok(checks) = studio.checks() {
                        for check in checks.iter().rev().filter(|r| r.task_id == task_id).take(5) {
                            ui.horizontal(|ui| {
                                let label =
                                    format!("#{} {:?}: {:?}", check.id, check.kind, check.status);
                                if ui.small_button(label).clicked() {
                                    self.selected_check = Some(check.id);
                                    self.show_diff = false;
                                }
                            });
                        }
                        if let Some(check_id) = self.selected_check {
                            if checks
                                .iter()
                                .any(|r| r.id == check_id && r.task_id == task_id)
                            {
                                if ui.small_button("Return to agent log").clicked() {
                                    self.selected_check = None;
                                }
                            } else {
                                self.selected_check = None;
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
            ui.selectable_value(&mut self.show_stderr, false, "Stdout");
            ui.selectable_value(&mut self.show_stderr, true, "Stderr");
        });
        ui.checkbox(&mut self.show_raw_events, "Raw JSONL");
        if task.mode == AgentMode::EditWorktree {
            ui.checkbox(&mut self.show_diff, "Worktree diff");
        } else {
            self.show_diff = false;
        }
        ui.separator();

        if self.selected_check.is_none() && self.show_diff {
            if task.status == TaskStatus::Running {
                ui.label("The worktree diff is available after the task finishes.");
            } else {
                self.worktree_diff(ui, task_id);
            }
            return;
        }

        if let Some(studio) = &self.studio {
            let output_result = if let Some(check_id) = self.selected_check {
                studio.check_log(check_id, self.show_stderr)
            } else {
                studio.read_log_tail(task_id, self.show_stderr)
            };
            match output_result {
                Ok(raw) => {
                    let output = if self.selected_check.is_none()
                        && !self.show_stderr
                        && !self.show_raw_events
                    {
                        let text = extract_agent_text(&raw);
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

    fn composer(&mut self, ui: &mut egui::Ui) {
        let active = self
            .studio
            .as_deref()
            .is_some_and(|studio| studio.is_active());
        ui.horizontal(|ui| {
            ui.colored_label(COLOR_ACCENT, "Codex");
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
        ui.checkbox(
            &mut self.edit_mode,
            "Allow edits in a separate Git worktree",
        );
        if self.edit_mode {
            ui.colored_label(
                COLOR_ERROR,
                "Requires a trusted Git repository. Changes stay in Studio's worktree until reviewed.",
            );
        }
        let edit = egui::TextEdit::multiline(&mut self.prompt)
            .hint_text("Ask Codex to inspect your project, explain an issue, or propose a plan...")
            .desired_rows(3);
        ui.add_enabled(!active && self.selected_bot.is_some(), edit);
        ui.horizontal(|ui| {
            let ready = !self.confirm_edit
                && !active
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
            if active {
                ui.spinner();
                ui.label("One task at a time");
            }
        });
    }
}

impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(
            if self
                .studio
                .as_deref()
                .is_some_and(|studio| studio.is_active())
            {
                350
            } else {
                1500
            },
        ));

        if let Some(error) = &self.error {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Kojora Studio could not open");
                ui.colored_label(COLOR_ERROR, error);
                ui.label("Check that the data directory is private and not locked by another Studio instance.");
            });
            return;
        }

        let state = match self.studio.as_ref().expect("checked above").snapshot() {
            Ok(state) => state,
            Err(error) => {
                self.notice = error.to_string();
                return;
            }
        };
        if self.selected_bot.is_none() {
            self.selected_bot = state.bots.first().map(|bot| bot.id);
        }

        egui::TopBottomPanel::top("top_bar")
            .min_height(68.0)
            .show(ctx, |ui| {
                ui.add_space(7.0);
                self.header(ui, &state);
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
            .show(ctx, |ui| self.inspector(ui, &state));

        egui::TopBottomPanel::bottom("composer")
            .resizable(false)
            .min_height(130.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.add_enabled_ui(!self.confirm_edit, |ui| self.composer(ui));
                ui.add_space(7.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| self.conversation(ui, &state));

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

fn main() -> eframe::Result<()> {
    let mut data_dir = match default_data_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("Could not determine studio data directory: {error}");
            return Ok(());
        }
    };
    let mut worker_socket = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--data-dir" => match args.next() {
                Some(path) => data_dir = PathBuf::from(path),
                None => {
                    eprintln!("--data-dir requires an absolute directory");
                    return Ok(());
                }
            },
            "--worker-socket" => match args.next() {
                Some(path) => worker_socket = Some(PathBuf::from(path)),
                None => {
                    eprintln!("--worker-socket requires an absolute socket path");
                    return Ok(());
                }
            },
            _ => {
                eprintln!("Usage: kojora-desktop [--data-dir ABSOLUTE_DIR] [--worker-socket ABSOLUTE_SOCKET]");
                return Ok(());
            }
        }
    }
    if !data_dir.is_absolute() || worker_socket.as_ref().is_some_and(|p| !p.is_absolute()) {
        eprintln!("Data directory and worker socket paths must be absolute");
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1250.0, 800.0])
            .with_min_inner_size([940.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Kojora Studio",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            install_cjk_fallback(&cc.egui_ctx);
            Ok(Box::new(DesktopApp::new(
                data_dir.clone(),
                worker_socket.clone(),
            )))
        }),
    )
}
