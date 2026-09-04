//! Desktop front end.
//!
//! The GUI owns no state of its own beyond form fields: it renders the latest
//! snapshot published by the bridge and sends commands back over a channel.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{Action, Config, Direction, PatchSource};
use crate::session::{self, SessionRequest};
use crate::patch::OutputGroup;
use crate::snapfile::SnapFile;
use crate::snapshot::{self, SnapshotRequest};
use crate::shared::{Command, CommandTx, LogBuffer, Shared, Snapshot};

const GREEN: egui::Color32 = egui::Color32::from_rgb(70, 180, 100);
const RED: egui::Color32 = egui::Color32::from_rgb(210, 90, 80);
const AMBER: egui::Color32 = egui::Color32::from_rgb(220, 170, 60);
const DIM: egui::Color32 = egui::Color32::from_rgb(140, 140, 150);

pub fn run(
    shared: Shared,
    tx: CommandTx,
    log: LogBuffer,
    cfg: Config,
    cfg_path: PathBuf,
    start_tab: Option<&str>,
    position: Option<[f32; 2]>,
) -> Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1060.0, 740.0])
        .with_min_inner_size([760.0, 520.0])
        .with_title("WING <-> LiveTrax Bridge");
    if let Some(pos) = position {
        viewport = viewport.with_position(pos);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let mut app = App::new(shared, tx, log, cfg, cfg_path);
    if let Some(tab) = start_tab {
        app.tab = Tab::from_name(tab)
            .with_context(|| format!("unknown tab {tab:?}"))?;
    }
    eframe::run_native(
        "WING <-> LiveTrax Bridge",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("GUI: {e}"))
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Channels,
    Transport,
    Scenes,
    NewSession,
    Snapshot,
    Log,
    Settings,
}

impl Tab {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_lowercase().replace(['-', '_'], "").as_str() {
            "channels" => Tab::Channels,
            "transport" => Tab::Transport,
            "scenes" => Tab::Scenes,
            "newsession" => Tab::NewSession,
            "snapshot" => Tab::Snapshot,
            "log" => Tab::Log,
            "settings" => Tab::Settings,
            _ => return None,
        })
    }
}

struct SessionForm {
    parent_dir: String,
    name: String,
    sample_rate: u32,
    template: String,
    templates: Vec<PathBuf>,
    connect_inputs: bool,
    allow_minimal: bool,
    first_ch: u16,
    last_ch: u16,
    include_unnamed: bool,
    /// Name tracks from the recorded output patch rather than raw channels.
    use_patch: bool,
}

impl SessionForm {
    fn new(cfg: &Config) -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            parent_dir: home,
            name: "WING Session".into(),
            sample_rate: cfg.livetrax.sample_rate as u32,
            template: cfg
                .livetrax
                .session_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            templates: session::discover_templates(),
            connect_inputs: true,
            allow_minimal: false,
            first_ch: 1,
            last_ch: cfg.wing.channels.min(32),
            include_unnamed: false,
            use_patch: true,
        }
    }
}

struct SnapForm {
    session: String,
    out: String,
    template: String,
    first_ch: u16,
    max_len: usize,
    include_busses: bool,
    /// Name onto the channels feeding the recorded outputs, not channel 1..N.
    use_patch: bool,
    /// Track names read from the session; the channel mapping is derived.
    tracks: Vec<String>,
    report: Option<Result<String, String>>,
    /// Read the configured session once, the first time the tab is opened.
    auto_read_done: bool,
}

impl SnapForm {
    fn new(cfg: &Config) -> Self {
        Self {
            session: cfg
                .livetrax
                .session_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            out: String::new(),
            template: String::new(),
            first_ch: cfg.snapshot.first_channel,
            max_len: cfg.names.max_len_wing,
            include_busses: cfg.snapshot.include_busses,
            use_patch: true,
            tracks: Vec::new(),
            report: None,
            auto_read_done: false,
        }
    }
}

/// The "which output feeds the DAW" selector.
struct PatchForm {
    source: PatchSource,
    file: String,
    group: String,
    groups: Vec<OutputGroup>,
    channel_names: usize,
    error: Option<String>,
    /// Parsed once on load, so previews do not re-read the file each frame.
    snap: Option<SnapFile>,
    /// Port groups a live console can be asked about.
    live_groups: Vec<String>,
    /// When a console query went out, so the UI can show it working.
    asked_at: Option<std::time::Instant>,
}

impl PatchForm {
    fn new(cfg: &Config) -> Self {
        let mut form = Self {
            source: cfg.patch.source,
            live_groups: cfg.patch.live.group_sizes.keys().cloned().collect(),
            asked_at: None,
            file: cfg
                .patch
                .snap_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            group: cfg.patch.output_group.clone(),
            groups: Vec::new(),
            channel_names: 0,
            error: None,
            snap: None,
        };
        form.reload();
        form
    }

    /// Read the .snap so the dropdown can list what the console actually has.
    fn reload(&mut self) {
        self.groups.clear();
        self.channel_names = 0;
        self.error = None;
        self.snap = None;
        let path = self.file.trim();
        if path.is_empty() {
            return;
        }
        match SnapFile::load(std::path::Path::new(path)) {
            Ok(snap) => {
                self.groups = snap.output_groups();
                self.channel_names = snap.channel_names().len();
                if !self.groups.iter().any(|g| g.id == self.group) {
                    if let Some(best) = self.groups.first() {
                        self.group = best.id.clone();
                    }
                }
                self.snap = Some(snap);
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }
}

struct App {
    shared: Shared,
    tx: CommandTx,
    log: LogBuffer,
    cfg: Config,
    cfg_path: PathBuf,
    tab: Tab,
    form: SessionForm,
    snap_form: SnapForm,
    patch_form: PatchForm,
    snap: Snapshot,
}

impl App {
    fn new(
        shared: Shared,
        tx: CommandTx,
        log: LogBuffer,
        cfg: Config,
        cfg_path: PathBuf,
    ) -> Self {
        Self {
            form: SessionForm::new(&cfg),
            snap_form: SnapForm::new(&cfg),
            patch_form: PatchForm::new(&cfg),
            shared,
            tx,
            log,
            cfg,
            cfg_path,
            tab: Tab::Channels,
            snap: Snapshot::default(),
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// Track names the new session would get, after de-duplication.
    ///
    /// With an output patch loaded the tracks follow the recorded outputs, so
    /// track N carries whatever the console sends on output N.
    fn planned_tracks(&self) -> Vec<String> {
        let mut raw = Vec::new();
        let use_patch = self.form.use_patch && !self.snap.patch_names.is_empty();
        for slot in self.form.first_ch..=self.form.last_ch.max(self.form.first_ch) {
            let name = if use_patch {
                self.snap
                    .patch_names
                    .iter()
                    .find(|(out, _)| *out == slot)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default()
            } else {
                self.snap.wing_names.get(&slot).cloned().unwrap_or_default()
            };
            let named = !name.trim().is_empty();
            if !named && !self.form.include_unnamed {
                continue;
            }
            let fallback = if use_patch { format!("Out {slot}") } else { format!("Ch {slot}") };
            raw.push(if named { name.trim().to_string() } else { fallback });
        }
        session::normalise_track_names(raw)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(200));
        if let Ok(s) = self.shared.lock() {
            self.snap = s.clone();
        }

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.small(format!(
                    "console {} | daw {} | {} msgs in",
                    self.snap.wing_target,
                    self.snap.daw_target,
                    self.snap.wing_msgs + self.snap.daw_msgs
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.small(format!("config: {}", self.cfg_path.display()));
                });
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Channels => self.channels_tab(ui),
            Tab::Transport => self.transport_tab(ui),
            Tab::Scenes => self.scenes_tab(ui),
            Tab::NewSession => self.session_tab(ui),
            Tab::Snapshot => self.snapshot_tab(ui),
            Tab::Log => self.log_tab(ui),
            Tab::Settings => self.settings_tab(ui),
        });
    }
}

// ------------------------------------------------------------------ views ---

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            link_pill(ui, "WING", self.snap.last_wing_rx);
            ui.add_space(8.0);
            link_pill(ui, "LiveTrax", self.snap.last_daw_rx);
            ui.separator();

            if ui.button(if self.snap.playing { "\u{23F8} Stop" } else { "\u{25B6} Play" }).clicked() {
                self.send(Command::Transport(if self.snap.playing {
                    Action::Stop
                } else {
                    Action::Play
                }));
            }
            dot(ui, if self.snap.recording { RED } else { DIM });
            if ui.button("Rec arm").clicked() {
                self.send(Command::Transport(Action::RecordArmToggle));
            }
            if ui.button("|< Start").clicked() {
                self.send(Command::Transport(Action::GotoStart));
            }
            if ui.button("Marker +").clicked() {
                self.send(Command::Transport(Action::AddMarker));
            }
            ui.separator();
            ui.monospace(timecode(self.snap.position, self.snap.sample_rate));
            if let Some(m) = &self.snap.current_marker {
                ui.label(egui::RichText::new(format!("\u{2691} {m}")).color(DIM));
            }
            if let Some(s) = self.snap.current_scene {
                ui.label(egui::RichText::new(format!("scene {s}")).color(DIM));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Channels, "Channels"),
                (Tab::Transport, "Transport"),
                (Tab::Scenes, "Scenes & markers"),
                (Tab::NewSession, "New session"),
                (Tab::Snapshot, "WING snapshot"),
                (Tab::Log, "Log"),
                (Tab::Settings, "Settings"),
            ] {
                ui.selectable_value(&mut self.tab, tab, label);
            }
        });
        ui.add_space(2.0);
    }

    fn channels_tab(&mut self, ui: &mut egui::Ui) {
        self.patch_selector(ui);
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Read names from console").clicked() {
                self.send(Command::QueryWingNames);
            }
            if ui.button("Push console -> DAW").clicked() {
                self.send(Command::PushNamesToDaw);
            }
            if ui.button("Push DAW -> console").clicked() {
                self.send(Command::PushNamesToWing);
            }
            if ui.button("Refresh strip list").clicked() {
                self.send(Command::RefreshDaw);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            egui::Grid::new("channels")
                .num_columns(5)
                .striped(true)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("Ch");
                    ui.strong("Console name");
                    ui.strong("Strip");
                    ui.strong("LiveTrax name");
                    ui.strong("");
                    ui.end_row();

                    for (ch, ssid) in &self.snap.pairs {
                        let wing = self.snap.wing_names.get(ch).cloned().unwrap_or_default();
                        let daw = self.snap.strips.get(ssid).cloned().unwrap_or_default();
                        ui.monospace(ch.to_string());
                        ui.label(&wing);
                        ui.monospace(ssid.to_string());
                        ui.label(&daw);
                        let (mark, color) = if wing.is_empty() && daw.is_empty() {
                            ("", DIM)
                        } else if !wing.is_empty() && wing == daw {
                            ("=", GREEN)
                        } else {
                            ("\u{2260}", AMBER)
                        };
                        ui.label(egui::RichText::new(mark).color(color));
                        ui.end_row();
                    }
                });
            if self.snap.pairs.is_empty() {
                ui.label("No channel map yet - check [map] in the config.");
            }
        });
    }

    /// Which console output the DAW records. The patch behind that output
    /// decides which channel's name belongs on which track.
    fn patch_selector(&mut self, ui: &mut egui::Ui) {
        let live = self.patch_form.source == PatchSource::Console;
        ui.horizontal(|ui| {
            ui.strong("Names follow output:");
            for (value, label) in [
                (PatchSource::Snap, "from .snap"),
                (PatchSource::Console, "from console"),
            ] {
                let picked = self.patch_form.source == value;
                if ui.selectable_label(picked, label).clicked() && !picked {
                    self.patch_form.source = value;
                    if value == PatchSource::Console {
                        self.query_console_patch();
                    } else {
                        self.patch_form.reload();
                        self.apply_patch();
                    }
                }
            }
            ui.separator();

            if live {
                let label = self.patch_form.group.clone();
                egui::ComboBox::from_id_salt("live_group")
                    .selected_text(label)
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        let groups = self.patch_form.live_groups.clone();
                        for id in groups {
                            let picked = self.patch_form.group == id;
                            if ui
                                .selectable_label(picked, format!("{:<4} {}", id, crate::patch::group_label(&id)))
                                .clicked()
                                && !picked
                            {
                                self.patch_form.group = id;
                                self.query_console_patch();
                            }
                        }
                    });
                if ui.button("Ask the console").clicked() {
                    self.query_console_patch();
                }
                if self
                    .patch_form
                    .asked_at
                    .map(|t| t.elapsed() < Duration::from_secs(4))
                    .unwrap_or(false)
                {
                    ui.spinner();
                }
            } else {
                ui.add(egui::TextEdit::singleline(&mut self.patch_form.file).desired_width(300.0));
                if ui.button("Browse\u{2026}").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("WING snapshot", &["snap"])
                        .pick_file()
                    {
                        self.patch_form.file = file.display().to_string();
                        self.patch_form.reload();
                        self.apply_patch();
                    }
                }
                let current = self
                    .patch_form
                    .groups
                    .iter()
                    .find(|g| g.id == self.patch_form.group);
                let label = match current {
                    Some(g) => format!("{} ({})", g.id, g.patched),
                    None => self.patch_form.group.clone(),
                };
                let enabled = !self.patch_form.groups.is_empty();
                ui.add_enabled_ui(enabled, |ui| {
                    egui::ComboBox::from_id_salt("output_group")
                        .selected_text(label)
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            let groups = self.patch_form.groups.clone();
                            for group in groups {
                                let picked = self.patch_form.group == group.id;
                                if ui
                                    .selectable_label(
                                        picked,
                                        format!("{:<4} {}", group.id, group.summary()),
                                    )
                                    .clicked()
                                    && !picked
                                {
                                    self.patch_form.group = group.id.clone();
                                    self.apply_patch();
                                }
                            }
                        });
                });
                if ui.button("Reload").clicked() {
                    self.patch_form.reload();
                    self.apply_patch();
                }
                if !self.patch_form.file.trim().is_empty() && ui.button("Clear").clicked() {
                    self.patch_form.file.clear();
                    self.patch_form.reload();
                    self.apply_patch();
                }
            }
        });

        if let Some(err) = &self.patch_form.error {
            ui.colored_label(RED, format!("error: {err}"));
        } else if let Some(summary) = &self.snap.patch_summary {
            let extra = if live {
                String::new()
            } else {
                format!("; {} named channels in the snapshot", self.patch_form.channel_names)
            };
            ui.small(format!(
                "{summary}{extra}. DAW strip N is output N of this group."
            ));
        } else if live {
            ui.small(
                "Waiting for the console. If nothing arrives, check wing.host and the \
                 [patch.live] addresses - `probe --target wing` shows what it really sends.",
            );
        } else {
            ui.small(
                "No output patch loaded - strips are mapped straight from channel numbers. \
                 Load a .snap saved from the console, or read the patch from the console itself.",
            );
        }
    }

    fn query_console_patch(&mut self) {
        self.patch_form.asked_at = Some(std::time::Instant::now());
        self.send(Command::QueryLivePatch {
            output_group: Some(self.patch_form.group.clone()),
        });
    }

    fn apply_patch(&mut self) {
        let file = self.patch_form.file.trim();
        self.send(Command::SetPatch {
            snap_file: (!file.is_empty()).then(|| PathBuf::from(file)),
            output_group: Some(self.patch_form.group.clone()),
        });
    }

    fn transport_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Transport");
        ui.horizontal_wrapped(|ui| {
            for (label, action) in [
                ("Play", Action::Play),
                ("Stop", Action::Stop),
                ("Toggle roll", Action::TogglePlay),
                ("Record arm", Action::RecordArmToggle),
                ("Record + roll", Action::RecordStart),
                ("Go to start", Action::GotoStart),
                ("Go to end", Action::GotoEnd),
                ("Previous marker", Action::PrevMarker),
                ("Next marker", Action::NextMarker),
                ("Drop marker", Action::AddMarker),
            ] {
                if ui.button(label).clicked() {
                    self.send(Command::Transport(action.clone()));
                }
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Position:");
            ui.monospace(timecode(self.snap.position, self.snap.sample_rate));
            ui.label(format!("({} samples @ {} Hz)", self.snap.position, self.snap.sample_rate));
        });
        ui.horizontal(|ui| {
            ui.label("State:");
            ui.colored_label(
                if self.snap.playing { GREEN } else { DIM },
                if self.snap.playing { "rolling" } else { "stopped" },
            );
            ui.colored_label(
                if self.snap.recording { RED } else { DIM },
                if self.snap.recording { "record armed" } else { "not armed" },
            );
        });
    }

    fn scenes_tab(&mut self, ui: &mut egui::Ui) {
        let mut scenes_enabled = self.snap.scenes_enabled;
        ui.horizontal(|ui| {
            if ui.checkbox(&mut scenes_enabled, "Scene linking enabled").changed() {
                self.send(Command::SetScenesEnabled(scenes_enabled));
            }
            if ui.button("Reload session file").clicked() {
                self.send(Command::ReloadSession);
            }
        });
        ui.small(match &self.snap.session_file {
            Some(p) => format!("markers from {}", p.display()),
            None => "no session file configured - marker positions are unknown".into(),
        });
        ui.separator();

        ui.strong("Scene <-> marker map");
        egui::Grid::new("scenes").num_columns(4).striped(true).show(ui, |ui| {
            for (scene, marker) in &self.snap.scene_map {
                let known = self
                    .snap
                    .markers
                    .iter()
                    .any(|(n, _)| n.eq_ignore_ascii_case(marker));
                ui.monospace(format!("scene {scene}"));
                ui.label(marker);
                ui.colored_label(
                    if known { GREEN } else { AMBER },
                    if known { "marker found" } else { "marker missing" },
                );
                ui.horizontal(|ui| {
                    if ui.small_button("Recall on console").clicked() {
                        self.send(Command::RecallScene(*scene));
                    }
                    if known && ui.small_button("Locate DAW").clicked() {
                        self.send(Command::LocateMarker(marker.clone()));
                    }
                });
                ui.end_row();
            }
        });
        if self.snap.scene_map.is_empty() {
            ui.label("No scene map - add entries under [scenes] in the config.");
        }

        ui.add_space(8.0);
        ui.strong(format!("Markers ({})", self.snap.markers.len()));
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            egui::Grid::new("markers").num_columns(3).striped(true).show(ui, |ui| {
                for (name, pos) in &self.snap.markers {
                    ui.label(name);
                    ui.monospace(timecode(*pos, self.snap.sample_rate));
                    if ui.small_button("Locate").clicked() {
                        self.send(Command::LocateMarker(name.clone()));
                    }
                    ui.end_row();
                }
            });
        });
    }

    fn session_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Create a LiveTrax session from the console");
        ui.small(
            "Builds a new session folder whose tracks are named after the WING channels. \
             A track from the template session is cloned per channel, so the route graph \
             matches whatever your LiveTrax version writes.",
        );
        ui.separator();

        egui::Grid::new("session_form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Destination folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.form.parent_dir).desired_width(420.0));
                if ui.button("Browse\u{2026}").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.form.parent_dir = dir.display().to_string();
                    }
                }
            });
            ui.end_row();

            ui.label("Session name");
            ui.add(egui::TextEdit::singleline(&mut self.form.name).desired_width(420.0));
            ui.end_row();

            ui.label("Sample rate");
            egui::ComboBox::from_id_salt("rate")
                .selected_text(format!("{} Hz", self.form.sample_rate))
                .show_ui(ui, |ui| {
                    for rate in [44_100u32, 48_000, 88_200, 96_000] {
                        ui.selectable_value(&mut self.form.sample_rate, rate, format!("{rate} Hz"));
                    }
                });
            ui.end_row();

            ui.label("Template session");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.form.template).desired_width(420.0));
                if ui.button("Browse\u{2026}").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("LiveTrax session", &["ardour", "template"])
                        .pick_file()
                    {
                        self.form.template = file.display().to_string();
                    }
                }
                if ui.button("Rescan").clicked() {
                    self.form.templates = session::discover_templates();
                }
            });
            ui.end_row();

            ui.label("");
            ui.vertical(|ui| {
                if self.form.templates.is_empty() {
                    ui.small("No installed templates found - point at any saved session that has at least one audio track.");
                } else {
                    egui::ComboBox::from_id_salt("templates")
                        .selected_text("Installed templates\u{2026}")
                        .show_ui(ui, |ui| {
                            let templates = self.form.templates.clone();
                            for t in templates {
                                if ui.selectable_label(false, t.display().to_string()).clicked() {
                                    self.form.template = t.display().to_string();
                                }
                            }
                        });
                }
            });
            ui.end_row();

            let patched = !self.snap.patch_names.is_empty();
            ui.label(if patched && self.form.use_patch { "Outputs" } else { "Channels" });
            ui.horizontal(|ui| {
                // Bounded by the configured channel count, not the snapshot,
                // so the form still works before the bridge has published.
                let max_ch = self.cfg.wing.channels.max(1);
                ui.add(egui::DragValue::new(&mut self.form.first_ch).range(1..=max_ch));
                ui.label("to");
                ui.add(egui::DragValue::new(&mut self.form.last_ch).range(1..=max_ch));
                ui.checkbox(&mut self.form.include_unnamed, "include unnamed");
                ui.add_enabled_ui(patched, |ui| {
                    ui.checkbox(&mut self.form.use_patch, "name from output patch");
                });
            });
            ui.end_row();
            if patched && self.form.use_patch {
                ui.label("");
                ui.small(
                    self.snap
                        .patch_summary
                        .clone()
                        .unwrap_or_default(),
                );
                ui.end_row();
            }

            ui.label("Options");
            ui.vertical(|ui| {
                ui.checkbox(
                    &mut self.form.connect_inputs,
                    "connect track inputs to system:capture_1..N",
                );
                ui.checkbox(
                    &mut self.form.allow_minimal,
                    "allow a synthesised session when no template is available (may not open)",
                );
            });
            ui.end_row();
        });

        ui.separator();
        let tracks = self.planned_tracks();
        ui.horizontal(|ui| {
            ui.strong(format!("{} tracks", tracks.len()));
            if ui.button("Read names from console").clicked() {
                self.send(Command::QueryWingNames);
            }
            let enabled = !tracks.is_empty() && !self.snap.session_busy;
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(
                        egui::RichText::new("Create LiveTrax session").strong(),
                    ),
                )
                .clicked()
            {
                let template = self.form.template.trim();
                self.send(Command::CreateSession(Box::new(SessionRequest {
                    parent_dir: PathBuf::from(self.form.parent_dir.trim()),
                    name: self.form.name.trim().to_string(),
                    sample_rate: self.form.sample_rate,
                    tracks: tracks.clone(),
                    template: (!template.is_empty()).then(|| PathBuf::from(template)),
                    connect_inputs: self.form.connect_inputs,
                    allow_minimal: self.form.allow_minimal,
                })));
            }
            if self.snap.session_busy {
                ui.spinner();
            }
        });

        egui::ScrollArea::vertical().max_height(160.0).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("preview").num_columns(2).striped(true).show(ui, |ui| {
                for (i, name) in tracks.iter().enumerate() {
                    ui.monospace(format!("{}", i + 1));
                    ui.label(name);
                    ui.end_row();
                }
            });
        });

        if let Some(report) = &self.snap.session_report {
            ui.separator();
            match report {
                Ok(r) => {
                    ui.colored_label(
                        GREEN,
                        format!("Created {} with {} tracks", r.session_file.display(), r.tracks),
                    );
                    ui.small(format!("folder: {}", r.folder.display()));
                    if let Some(t) = &r.template {
                        ui.small(format!("cloned from {}", t.display()));
                    }
                    for w in &r.warnings {
                        ui.colored_label(AMBER, format!("warning: {w}"));
                    }
                    ui.small("Open it in LiveTrax with Session > Open. The bridge is now following this session for markers.");
                }
                Err(e) => {
                    ui.colored_label(RED, format!("error: {e}"));
                }
            }
        }
    }

    fn snapshot_tab(&mut self, ui: &mut egui::Ui) {
        if !self.snap_form.auto_read_done {
            self.snap_form.auto_read_done = true;
            if !self.snap_form.session.trim().is_empty() {
                self.read_session_tracks();
            }
        }
        ui.heading("Offline WING snapshot from a LiveTrax session");
        ui.small(
            "Reads track names from a session file - no DAW or console needed - and writes them \
             as console channel names. Behringer's snapshot container is undocumented, so the \
             output is node text: one address/value line per channel, which this tool can also \
             push straight to the console. Point Template at a snapshot exported from your own \
             WING (if it is a text file) to rewrite only its name entries.",
        );
        ui.separator();

        egui::Grid::new("snap_form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Session file");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.snap_form.session).desired_width(420.0));
                if ui.button("Browse\u{2026}").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("LiveTrax session", &["ardour"])
                        .pick_file()
                    {
                        self.snap_form.session = file.display().to_string();
                    }
                }
                if ui.button("Read tracks").clicked() {
                    self.read_session_tracks();
                }
            });
            ui.end_row();

            ui.label("Mapping");
            ui.horizontal(|ui| {
                let patched = self.patch_form.snap.is_some();
                ui.add_enabled_ui(patched, |ui| {
                    ui.checkbox(&mut self.snap_form.use_patch, "via output patch");
                });
                ui.label(if patched && self.snap_form.use_patch {
                    "first output"
                } else {
                    "first console channel"
                });
                ui.add(
                    egui::DragValue::new(&mut self.snap_form.first_ch)
                        .range(1..=self.cfg.wing.channels.max(1)),
                );
                ui.label("name length");
                ui.add(egui::DragValue::new(&mut self.snap_form.max_len).range(0..=32));
                if ui
                    .checkbox(&mut self.snap_form.include_busses, "include busses")
                    .changed()
                    && !self.snap_form.tracks.is_empty()
                {
                    self.read_session_tracks();
                }
            });
            ui.end_row();

            ui.label("Template .snap");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.snap_form.template).desired_width(420.0));
                if ui.button("Browse\u{2026}").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("WING snapshot", &["snap"])
                        .pick_file()
                    {
                        self.snap_form.template = file.display().to_string();
                    }
                }
                if !self.patch_form.file.trim().is_empty()
                    && ui.button("Use loaded patch file").clicked()
                {
                    self.snap_form.template = self.patch_form.file.clone();
                }
            });
            ui.end_row();

            ui.label("Output file");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.snap_form.out).desired_width(420.0));
                if ui.button("Browse\u{2026}").clicked() {
                    let default = format!(
                        "{}-wing.txt",
                        std::path::Path::new(&self.snap_form.session)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "snapshot".into())
                    );
                    if let Some(file) = rfd::FileDialog::new().set_file_name(default).save_file() {
                        self.snap_form.out = file.display().to_string();
                    }
                }
            });
            ui.end_row();
        });

        let via_patch = self.snap_form.use_patch && self.patch_form.snap.is_some();
        let (entries, skipped) = match (&self.patch_form.snap, via_patch) {
            (Some(snap), true) => snapshot::map_entries_via_patch(
                self.snap_form.tracks.clone(),
                snap,
                &self.patch_form.group,
                self.snap_form.first_ch,
                self.snap_form.max_len,
            ),
            _ => (
                snapshot::map_entries(
                    self.snap_form.tracks.clone(),
                    self.snap_form.first_ch,
                    self.snap_form.max_len,
                ),
                Vec::new(),
            ),
        };

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong(format!("{} channels", entries.len()));
            let ready = !entries.is_empty();
            if ui
                .add_enabled(
                    ready && !self.snap_form.out.trim().is_empty(),
                    egui::Button::new(egui::RichText::new("Write snapshot file").strong()),
                )
                .clicked()
            {
                self.write_snapshot(&entries);
            }
            if ui
                .add_enabled(ready, egui::Button::new("Apply names to console now"))
                .clicked()
            {
                self.send(Command::ApplyChannelNames(
                    entries.iter().map(|e| (e.channel, e.name.clone())).collect(),
                ));
                self.snap_form.report = Some(Ok(format!(
                    "sent {} channel names to the console",
                    entries.len()
                )));
            }
            if entries.is_empty() {
                ui.small("Read a session first.");
            }
            if !skipped.is_empty() {
                ui.colored_label(
                    AMBER,
                    format!("{} tracks skipped: their output carries no channel", skipped.len()),
                );
            }
        });

        if let Some(report) = &self.snap_form.report {
            match report {
                Ok(msg) => ui.colored_label(GREEN, msg.clone()),
                Err(e) => ui.colored_label(RED, format!("error: {e}")),
            };
        }

        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            egui::Grid::new("snap_preview").num_columns(4).striped(true).show(ui, |ui| {
                ui.strong("Out");
                ui.strong("Ch");
                ui.strong("Track");
                ui.strong("Console name");
                ui.end_row();
                for e in &entries {
                    ui.monospace(e.output.map(|o| o.to_string()).unwrap_or_else(|| "-".into()));
                    ui.monospace(e.channel.to_string());
                    ui.label(&e.track);
                    let truncated = e.name != e.track;
                    ui.label(
                        egui::RichText::new(&e.name).color(if truncated { AMBER } else { ui.visuals().text_color() }),
                    );
                    ui.end_row();
                }
            });
        });
    }

    fn read_session_tracks(&mut self) {
        let path = PathBuf::from(self.snap_form.session.trim());
        match crate::markers::resolve_session_path(&path)
            .and_then(|p| snapshot::read_tracks(&p, self.snap_form.include_busses))
        {
            Ok(tracks) => {
                self.snap_form.report =
                    Some(Ok(format!("read {} tracks from the session", tracks.len())));
                self.snap_form.tracks = tracks;
            }
            Err(e) => {
                self.snap_form.tracks.clear();
                self.snap_form.report = Some(Err(format!("{e:#}")));
            }
        }
    }

    fn write_snapshot(&mut self, entries: &[snapshot::Entry]) {
        let template = self.snap_form.template.trim();
        let req = SnapshotRequest {
            session: PathBuf::from(self.snap_form.session.trim()),
            first_channel: self.snap_form.first_ch,
            max_len: self.snap_form.max_len,
            include_busses: self.snap_form.include_busses,
            template: (!template.is_empty()).then(|| PathBuf::from(template)),
            output_group: (self.snap_form.use_patch && self.patch_form.snap.is_some())
                .then(|| self.patch_form.group.clone()),
        };
        let plan = snapshot::SnapshotPlan {
            session_name: std::path::Path::new(&self.snap_form.session)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            entries: entries.to_vec(),
            skipped: Vec::new(),
        };
        let out = PathBuf::from(self.snap_form.out.trim());
        self.snap_form.report = match snapshot::write(
            &plan,
            &req,
            &self.cfg.snapshot,
            &self.cfg.wing,
            &out,
        ) {
            Ok(report) => {
                let mut msg = format!("wrote {} channels to {}", report.written, report.path.display());
                if !report.unmatched.is_empty() {
                    msg.push_str(&format!(
                        " - no line in the template for channels {:?}",
                        report.unmatched
                    ));
                }
                Some(Ok(msg))
            }
            Err(e) => Some(Err(format!("{e:#}"))),
        };
    }

    fn log_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Log");
            if ui.button("Clear").clicked() {
                self.log.clear();
            }
        });
        ui.separator();
        let lines = self.log.lines();
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in lines {
                    let color = if line.contains("ERROR") {
                        RED
                    } else if line.contains("WARN") {
                        AMBER
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.label(egui::RichText::new(line).monospace().color(color));
                }
            });
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.small("Host, port and address settings live in the config file; edit it and restart.");
        ui.separator();

        let mut names_enabled = self.snap.names_enabled;
        if ui.checkbox(&mut names_enabled, "Channel name sync").changed() {
            self.send(Command::SetNamesEnabled(names_enabled));
        }
        ui.horizontal(|ui| {
            ui.label("Direction:");
            let mut dir = self.snap.names_direction;
            for (value, label) in [
                (Direction::WingToDaw, "console -> DAW"),
                (Direction::DawToWing, "DAW -> console"),
                (Direction::Bidirectional, "both"),
            ] {
                if ui.selectable_value(&mut dir, value, label).clicked() {
                    self.send(Command::SetNamesDirection(value));
                }
            }
        });

        let mut scenes_enabled = self.snap.scenes_enabled;
        if ui.checkbox(&mut scenes_enabled, "Scene <-> marker linking").changed() {
            self.send(Command::SetScenesEnabled(scenes_enabled));
        }

        ui.separator();
        egui::Grid::new("settings").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            ui.label("Console");
            ui.monospace(format!("{}:{}", self.cfg.wing.host, self.cfg.wing.port));
            ui.end_row();
            ui.label("LiveTrax");
            ui.monospace(format!("{}:{}", self.cfg.livetrax.host, self.cfg.livetrax.port));
            ui.end_row();
            ui.label("Channel name address");
            ui.monospace(&self.cfg.wing.name_address);
            ui.end_row();
            ui.label("Scene address");
            ui.monospace(&self.cfg.scenes.scene_address);
            ui.end_row();
            ui.label("Session file");
            ui.horizontal(|ui| {
                ui.monospace(
                    self.snap
                        .session_file
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "-".into()),
                );
                if ui.small_button("Choose\u{2026}").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("LiveTrax session", &["ardour"])
                        .pick_file()
                    {
                        self.send(Command::SetSessionFile(file));
                    }
                }
            });
            ui.end_row();
        });

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Save settings to config file").clicked() {
                self.send(Command::SaveConfig(self.cfg_path.clone()));
            }
            ui.small("Rewrites the TOML from the running configuration; comments are lost.");
        });
    }
}

// ----------------------------------------------------------------- helpers --

fn link_pill(ui: &mut egui::Ui, label: &str, last: Option<std::time::Instant>) {
    let (color, detail) = match last {
        Some(t) if t.elapsed() < Duration::from_secs(10) => {
            (GREEN, format!("{:.0}s ago", t.elapsed().as_secs_f32()))
        }
        Some(t) => (AMBER, format!("{:.0}s ago", t.elapsed().as_secs_f32())),
        None => (RED, "no traffic".to_string()),
    };
    dot(ui, color);
    ui.colored_label(color, label);
    ui.small(egui::RichText::new(detail).color(DIM));
}

/// A status dot, painted rather than drawn from a font: the bundled egui fonts
/// have no glyph for it.
fn dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn timecode(samples: i64, rate: f64) -> String {
    let rate = if rate > 1.0 { rate } else { 48_000.0 };
    let total = samples.max(0) as f64 / rate;
    let h = (total / 3600.0).floor() as u64;
    let m = ((total % 3600.0) / 60.0).floor() as u64;
    let s = total % 60.0;
    format!("{h:02}:{m:02}:{s:06.3}")
}
