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

use crate::theme::{self, ACCENT, AMBER, DIM, GREEN, RED, TEXT};

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
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
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
        let use_patch = self.form.use_patch && !self.snap.patch_slots.is_empty();
        for slot in self.form.first_ch..=self.form.last_ch.max(self.form.first_ch) {
            let name = if use_patch {
                self.snap
                    .patch_slots
                    .iter()
                    .find(|s| s.output == slot)
                    .map(|s| s.name.clone())
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

        egui::Panel::top("header")
            .frame(bar_frame())
            .show(ui, |ui| self.header(ui));
        egui::Panel::top("tabs")
            .frame(tabs_frame())
            .show(ui, |ui| self.tab_bar(ui));
        egui::Panel::bottom("status")
            .frame(bar_frame())
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 10)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| match self.tab {
                        Tab::Channels => self.channels_tab(ui),
                        Tab::Transport => self.transport_tab(ui),
                        Tab::Scenes => self.scenes_tab(ui),
                        Tab::NewSession => self.session_tab(ui),
                        Tab::Snapshot => self.snapshot_tab(ui),
                        Tab::Log => self.log_tab(ui),
                        Tab::Settings => self.settings_tab(ui),
                    });
            });
    }
}

// ------------------------------------------------------------ chrome -------

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            theme::pill(ui, link_color(self.snap.last_wing_rx), "WING", &age(self.snap.last_wing_rx));
            theme::pill(
                ui,
                link_color(self.snap.last_daw_rx),
                "LiveTrax",
                &age(self.snap.last_daw_rx),
            );
            ui.add_space(6.0);

            let rolling = self.snap.playing;
            if ui.add(theme::primary(if rolling { "Stop" } else { "Play" })).clicked() {
                self.send(Command::Transport(if rolling { Action::Stop } else { Action::Play }));
            }
            theme::dot(ui, if self.snap.recording { RED } else { theme::LINE });
            if ui.button("Rec arm").clicked() {
                self.send(Command::Transport(Action::RecordArmToggle));
            }
            if ui.button("|< Start").clicked() {
                self.send(Command::Transport(Action::GotoStart));
            }
            if ui.button("Marker +").clicked() {
                self.send(Command::Transport(Action::AddMarker));
            }

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(timecode(self.snap.position, self.snap.sample_rate))
                    .monospace()
                    .size(15.0)
                    .color(if rolling { GREEN } else { TEXT }),
            );

            if let Some(marker) = self.snap.current_marker.clone() {
                chip(ui, &format!("marker  {marker}"));
            }
            if let Some(scene) = self.snap.current_scene {
                chip(ui, &format!("scene  {scene}"));
            }
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for (tab, label) in [
                (Tab::Channels, "Channels"),
                (Tab::Transport, "Transport"),
                (Tab::Scenes, "Scenes & markers"),
                (Tab::NewSession, "New session"),
                (Tab::Snapshot, "WING snapshot"),
                (Tab::Log, "Log"),
                (Tab::Settings, "Settings"),
            ] {
                let selected = self.tab == tab;
                let text = egui::RichText::new(label).color(if selected { TEXT } else { DIM });
                if ui.selectable_label(selected, text).clicked() {
                    self.tab = tab;
                }
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "console {}   daw {}   {} messages in",
                    self.snap.wing_target,
                    self.snap.daw_target,
                    self.snap.wing_msgs + self.snap.daw_msgs
                ))
                .small()
                .color(DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(self.cfg_path.display().to_string())
                        .small()
                        .color(DIM),
                );
            });
        });
    }
}

// ------------------------------------------------------------- tabs --------

impl App {
    fn channels_tab(&mut self, ui: &mut egui::Ui) {
        theme::titled_card(ui, "RECORDED OUTPUT", |ui| self.patch_selector(ui));
        ui.add_space(10.0);

        theme::titled_card(ui, "CHANNEL NAMES", |ui| {
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
            ui.add_space(6.0);

            if self.snap.pairs.is_empty() {
                theme::empty(ui, "No channel map yet - load an output patch, or set [map] in the config.");
                return;
            }
            let patched = !self.snap.patch_slots.is_empty();
            egui::Grid::new("channels")
                .num_columns(if patched { 6 } else { 5 })
                .striped(true)
                .spacing([12.0, 5.0])
                .show(ui, |ui| {
                    theme::column(ui, "CH");
                    theme::column(ui, "CONSOLE NAME");
                    if patched {
                        theme::column(ui, "PATCHED FROM");
                    }
                    theme::column(ui, "STRIP");
                    theme::column(ui, "LIVETRAX NAME");
                    theme::column(ui, "");
                    ui.end_row();

                    for (ch, ssid) in &self.snap.pairs {
                        let wing = self.snap.wing_names.get(ch).cloned().unwrap_or_default();
                        let daw = self.snap.strips.get(ssid).cloned().unwrap_or_default();
                        theme::num(ui, ch.to_string());
                        cell(ui, 150.0, egui::RichText::new(&wing).color(TEXT));
                        if patched {
                            let source = self
                                .snap
                                .patch_slots
                                .iter()
                                .find(|s| s.channel == Some(*ch))
                                .map(|s| s.source.clone())
                                .unwrap_or_default();
                            cell(ui, 90.0, egui::RichText::new(source).color(DIM).monospace());
                        }
                        theme::num(ui, ssid.to_string());
                        cell(ui, 150.0, egui::RichText::new(&daw).color(TEXT));
                        let (mark, color) = if wing.is_empty() && daw.is_empty() {
                            ("", DIM)
                        } else if !wing.is_empty() && wing == daw {
                            ("=", GREEN)
                        } else {
                            ("!=", AMBER)
                        };
                        ui.label(egui::RichText::new(mark).color(color).monospace());
                        ui.end_row();
                    }
                });
        });
    }

    /// Which console output the DAW records. The patch behind that output
    /// decides which channel's name belongs on which track.
    fn patch_selector(&mut self, ui: &mut egui::Ui) {
        let live = self.patch_form.source == PatchSource::Console;
        ui.horizontal(|ui| {
            theme::field(ui, "Read the patch");
            for (value, label) in [
                (PatchSource::Snap, "from a .snap"),
                (PatchSource::Console, "from the console"),
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
        });

        if live {
            ui.horizontal(|ui| {
                theme::field(ui, "Output group");
                let label = self.patch_form.group.clone();
                egui::ComboBox::from_id_salt("live_group")
                    .selected_text(label)
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        let groups = self.patch_form.live_groups.clone();
                        for id in groups {
                            let picked = self.patch_form.group == id;
                            let text = format!("{:<4} {}", id, crate::patch::group_label(&id));
                            if ui.selectable_label(picked, text).clicked() && !picked {
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
            });
        } else {
            ui.horizontal(|ui| {
                theme::field(ui, "Snapshot file");
                ui.add(egui::TextEdit::singleline(&mut self.patch_form.file).desired_width(330.0));
                if ui.button("Browse").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("WING snapshot", &["snap"])
                        .pick_file()
                    {
                        self.patch_form.file = file.display().to_string();
                        self.patch_form.reload();
                        self.apply_patch();
                    }
                }
                if ui.button("Reload").clicked() {
                    self.patch_form.reload();
                    self.apply_patch();
                }
                if !self.patch_form.file.trim().is_empty() && ui.button("Clear").clicked() {
                    self.patch_form.file.clear();
                    self.patch_form.reload();
                    self.apply_patch();
                }
            });
            ui.horizontal(|ui| {
                theme::field(ui, "Output group");
                let current = self
                    .patch_form
                    .groups
                    .iter()
                    .find(|g| g.id == self.patch_form.group);
                let label = match current {
                    Some(g) => format!("{}  ({} patched)", g.id, g.patched),
                    None => self.patch_form.group.clone(),
                };
                let enabled = !self.patch_form.groups.is_empty();
                ui.add_enabled_ui(enabled, |ui| {
                    egui::ComboBox::from_id_salt("output_group")
                        .selected_text(label)
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            let groups = self.patch_form.groups.clone();
                            for group in groups {
                                let picked = self.patch_form.group == group.id;
                                let text = format!("{:<4} {}", group.id, group.summary());
                                if ui.selectable_label(picked, text).clicked() && !picked {
                                    self.patch_form.group = group.id.clone();
                                    self.apply_patch();
                                }
                            }
                        });
                });
            });
        }

        ui.add_space(2.0);
        if let Some(err) = &self.patch_form.error {
            ui.label(egui::RichText::new(format!("error: {err}")).color(RED).small());
        } else if let Some(summary) = &self.snap.patch_summary {
            let extra = if live {
                String::new()
            } else {
                format!("; {} named channels", self.patch_form.channel_names)
            };
            ui.label(
                egui::RichText::new(format!(
                    "{summary}{extra}. DAW strip N is output N of this group."
                ))
                .color(DIM)
                .small(),
            );
        } else if live {
            ui.label(
                egui::RichText::new(
                    "Waiting for the console. If nothing arrives, check wing.host and the \
                     [patch.live] addresses - `probe --target wing` shows what it really sends.",
                )
                .color(AMBER)
                .small(),
            );
        } else {
            ui.label(
                egui::RichText::new(
                    "No patch loaded - strips are mapped straight from channel numbers.",
                )
                .color(DIM)
                .small(),
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
        theme::titled_card(ui, "STATE", |ui| {
            ui.horizontal(|ui| {
                theme::dot(ui, if self.snap.playing { GREEN } else { theme::LINE });
                ui.label(if self.snap.playing { "rolling" } else { "stopped" });
                ui.add_space(10.0);
                theme::dot(ui, if self.snap.recording { RED } else { theme::LINE });
                ui.label(if self.snap.recording { "record armed" } else { "not armed" });
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(timecode(self.snap.position, self.snap.sample_rate))
                        .monospace()
                        .size(15.0),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "{} samples @ {} Hz",
                        self.snap.position, self.snap.sample_rate
                    ))
                    .small()
                    .color(DIM),
                );
            });
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "SEND TO LIVETRAX", |ui| {
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
        });
    }

    fn scenes_tab(&mut self, ui: &mut egui::Ui) {
        theme::titled_card(ui, "SCENE LINKING", |ui| {
            ui.horizontal(|ui| {
                let mut enabled = self.snap.scenes_enabled;
                if ui.checkbox(&mut enabled, "Enabled").changed() {
                    self.send(Command::SetScenesEnabled(enabled));
                }
                if ui.button("Reload session file").clicked() {
                    self.send(Command::ReloadSession);
                }
            });
            ui.label(
                egui::RichText::new(match &self.snap.session_file {
                    Some(p) => format!("markers from {}", p.display()),
                    None => "no session file configured - marker positions are unknown".into(),
                })
                .small()
                .color(DIM),
            );
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "SCENE <-> MARKER MAP", |ui| {
            if self.snap.scene_map.is_empty() {
                theme::empty(ui, "No scene map - add entries under [scenes] in the config.");
                return;
            }
            egui::Grid::new("scenes").num_columns(4).striped(true).spacing([12.0, 5.0]).show(
                ui,
                |ui| {
                    theme::column(ui, "SCENE");
                    theme::column(ui, "MARKER");
                    theme::column(ui, "");
                    theme::column(ui, "");
                    ui.end_row();
                    for (scene, marker) in &self.snap.scene_map {
                        let known = self
                            .snap
                            .markers
                            .iter()
                            .any(|(n, _)| n.eq_ignore_ascii_case(marker));
                        theme::num(ui, scene.to_string());
                        cell(ui, 160.0, egui::RichText::new(marker).color(TEXT));
                        ui.label(
                            egui::RichText::new(if known { "marker found" } else { "not in session" })
                                .small()
                                .color(if known { GREEN } else { AMBER }),
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
                },
            );
        });
        ui.add_space(10.0);

        theme::titled_card(ui, &format!("MARKERS ({})", self.snap.markers.len()), |ui| {
            if self.snap.markers.is_empty() {
                theme::empty(ui, "No markers - point at a saved session, or drop one from the header.");
                return;
            }
            egui::Grid::new("markers").num_columns(3).striped(true).spacing([12.0, 5.0]).show(
                ui,
                |ui| {
                    for (name, pos) in &self.snap.markers {
                        cell(ui, 200.0, egui::RichText::new(name).color(TEXT));
                        theme::num(ui, timecode(*pos, self.snap.sample_rate));
                        if ui.small_button("Locate").clicked() {
                            self.send(Command::LocateMarker(name.clone()));
                        }
                        ui.end_row();
                    }
                },
            );
        });
    }

    fn session_tab(&mut self, ui: &mut egui::Ui) {
        let patched = !self.snap.patch_slots.is_empty();
        theme::titled_card(ui, "TRACK NAMES FROM", |ui| {
            ui.horizontal(|ui| {
                theme::field(ui, if patched && self.form.use_patch { "Outputs" } else { "Channels" });
                let max_ch = self.cfg.wing.channels.max(1);
                ui.add(egui::DragValue::new(&mut self.form.first_ch).range(1..=max_ch));
                ui.label(egui::RichText::new("to").color(DIM));
                ui.add(egui::DragValue::new(&mut self.form.last_ch).range(1..=max_ch));
                ui.add_space(8.0);
                ui.checkbox(&mut self.form.include_unnamed, "include unnamed");
                ui.add_enabled_ui(patched, |ui| {
                    ui.checkbox(&mut self.form.use_patch, "follow the output patch");
                });
            });
            if patched && self.form.use_patch {
                if let Some(summary) = &self.snap.patch_summary {
                    ui.label(egui::RichText::new(summary).small().color(DIM));
                }
            } else if !patched {
                ui.label(
                    egui::RichText::new("No patch loaded, so tracks follow console channel order.")
                        .small()
                        .color(DIM),
                );
            }
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "NEW SESSION", |ui| {
            ui.horizontal(|ui| {
                theme::field(ui, "Destination folder");
                ui.add(egui::TextEdit::singleline(&mut self.form.parent_dir).desired_width(360.0));
                if ui.button("Browse").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.form.parent_dir = dir.display().to_string();
                    }
                }
            });
            ui.horizontal(|ui| {
                theme::field(ui, "Session name");
                ui.add(egui::TextEdit::singleline(&mut self.form.name).desired_width(360.0));
            });
            ui.horizontal(|ui| {
                theme::field(ui, "Sample rate");
                egui::ComboBox::from_id_salt("rate")
                    .selected_text(format!("{} Hz", self.form.sample_rate))
                    .width(140.0)
                    .show_ui(ui, |ui| {
                        for rate in [44_100u32, 48_000, 88_200, 96_000] {
                            ui.selectable_value(&mut self.form.sample_rate, rate, format!("{rate} Hz"));
                        }
                    });
            });
            ui.horizontal(|ui| {
                theme::field(ui, "Template session");
                ui.add(egui::TextEdit::singleline(&mut self.form.template).desired_width(360.0));
                if ui.button("Browse").clicked() {
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
            if !self.form.templates.is_empty() {
                ui.horizontal(|ui| {
                    theme::field(ui, "");
                    egui::ComboBox::from_id_salt("templates")
                        .selected_text("Installed templates")
                        .width(360.0)
                        .show_ui(ui, |ui| {
                            let templates = self.form.templates.clone();
                            for t in templates {
                                if ui.selectable_label(false, t.display().to_string()).clicked() {
                                    self.form.template = t.display().to_string();
                                }
                            }
                        });
                });
            } else {
                ui.horizontal(|ui| {
                    theme::field(ui, "");
                    ui.label(
                        egui::RichText::new(
                            "No installed templates found - point at any saved session with an audio track.",
                        )
                        .small()
                        .color(DIM),
                    );
                });
            }
            ui.horizontal(|ui| {
                theme::field(ui, "Options");
                ui.vertical(|ui| {
                    ui.checkbox(&mut self.form.connect_inputs, "connect inputs to system:capture_1..N");
                    ui.checkbox(
                        &mut self.form.allow_minimal,
                        "allow a synthesised session with no template (may not open)",
                    );
                });
            });
        });
        ui.add_space(10.0);

        let tracks = self.planned_tracks();
        ui.horizontal(|ui| {
            let enabled = !tracks.is_empty() && !self.snap.session_busy;
            if ui.add_enabled(enabled, theme::primary("Create LiveTrax session")).clicked() {
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
            ui.label(egui::RichText::new(format!("{} tracks", tracks.len())).color(DIM));
            if ui.button("Read names from console").clicked() {
                self.send(Command::QueryWingNames);
            }
            if self.snap.session_busy {
                ui.spinner();
            }
        });

        if let Some(report) = &self.snap.session_report {
            ui.add_space(6.0);
            theme::card(ui, |ui| match report {
                Ok(r) => {
                    ui.label(
                        egui::RichText::new(format!(
                            "Created {} with {} tracks",
                            r.session_file.display(),
                            r.tracks
                        ))
                        .color(GREEN),
                    );
                    ui.label(
                        egui::RichText::new(format!("folder: {}", r.folder.display()))
                            .small()
                            .color(DIM),
                    );
                    if let Some(t) = &r.template {
                        ui.label(egui::RichText::new(format!("cloned from {}", t.display())).small().color(DIM));
                    }
                    for w in &r.warnings {
                        ui.label(egui::RichText::new(format!("warning: {w}")).small().color(AMBER));
                    }
                    ui.label(
                        egui::RichText::new(
                            "Open it in LiveTrax with Session > Open. The bridge now follows it for markers.",
                        )
                        .small()
                        .color(DIM),
                    );
                }
                Err(e) => {
                    ui.label(egui::RichText::new(format!("error: {e}")).color(RED));
                }
            });
        }

        ui.add_space(10.0);
        theme::titled_card(ui, "TRACKS TO BE CREATED", |ui| {
            if tracks.is_empty() {
                theme::empty(ui, "Nothing to create - read names from the console, or tick \"include unnamed\".");
                return;
            }
            egui::Grid::new("preview").num_columns(2).striped(true).spacing([12.0, 4.0]).show(
                ui,
                |ui| {
                    for (i, name) in tracks.iter().enumerate() {
                        theme::num(ui, format!("{}", i + 1));
                        cell(ui, 260.0, egui::RichText::new(name).color(TEXT));
                        ui.end_row();
                    }
                },
            );
        });
    }

    fn snapshot_tab(&mut self, ui: &mut egui::Ui) {
        if !self.snap_form.auto_read_done {
            self.snap_form.auto_read_done = true;
            if !self.snap_form.session.trim().is_empty() {
                self.read_session_tracks();
            }
        }

        theme::titled_card(ui, "FROM A LIVETRAX SESSION", |ui| {
            ui.label(
                egui::RichText::new(
                    "Track names go the other way: out of a session file and onto the console. \
                     With a .snap template the result is a genuine snapshot - only the names change.",
                )
                .small()
                .color(DIM),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                theme::field(ui, "Session file");
                ui.add(egui::TextEdit::singleline(&mut self.snap_form.session).desired_width(360.0));
                if ui.button("Browse").clicked() {
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
            ui.horizontal(|ui| {
                let patched = self.patch_form.snap.is_some();
                theme::field(ui, "Mapping");
                ui.add_enabled_ui(patched, |ui| {
                    ui.checkbox(&mut self.snap_form.use_patch, "via output patch");
                });
                ui.label(
                    egui::RichText::new(if patched && self.snap_form.use_patch {
                        "first output"
                    } else {
                        "first channel"
                    })
                    .color(DIM),
                );
                let max_ch = self.cfg.wing.channels.max(1);
                ui.add(egui::DragValue::new(&mut self.snap_form.first_ch).range(1..=max_ch));
                ui.label(egui::RichText::new("name length").color(DIM));
                ui.add(egui::DragValue::new(&mut self.snap_form.max_len).range(0..=32));
                ui.checkbox(&mut self.snap_form.include_busses, "include busses");
            });
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "WRITE", |ui| {
            ui.horizontal(|ui| {
                theme::field(ui, "Template .snap");
                ui.add(egui::TextEdit::singleline(&mut self.snap_form.template).desired_width(360.0));
                if ui.button("Browse").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("WING snapshot", &["snap"])
                        .pick_file()
                    {
                        self.snap_form.template = file.display().to_string();
                    }
                }
                if !self.patch_form.file.trim().is_empty() && ui.button("Use the loaded one").clicked() {
                    self.snap_form.template = self.patch_form.file.clone();
                }
            });
            ui.horizontal(|ui| {
                theme::field(ui, "Save as");
                ui.add(egui::TextEdit::singleline(&mut self.snap_form.out).desired_width(360.0));
                if ui.button("Browse").clicked() {
                    let stem = std::path::Path::new(&self.snap_form.session)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "snapshot".into());
                    let is_snap = self.snap_form.template.trim().to_lowercase().ends_with(".snap");
                    let default = if is_snap { format!("{stem}.snap") } else { format!("{stem}-wing.txt") };
                    if let Some(file) = rfd::FileDialog::new().set_file_name(default).save_file() {
                        self.snap_form.out = file.display().to_string();
                    }
                }
            });
        });
        ui.add_space(10.0);

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

        ui.horizontal(|ui| {
            let ready = !entries.is_empty();
            if ui
                .add_enabled(
                    ready && !self.snap_form.out.trim().is_empty(),
                    theme::primary("Write snapshot"),
                )
                .clicked()
            {
                self.write_snapshot(&entries);
            }
            if ui.add_enabled(ready, egui::Button::new("Apply names to console now")).clicked() {
                self.send(Command::ApplyChannelNames(
                    entries.iter().map(|e| (e.channel, e.name.clone())).collect(),
                ));
                self.snap_form.report =
                    Some(Ok(format!("sent {} channel names to the console", entries.len())));
            }
            ui.label(egui::RichText::new(format!("{} channels", entries.len())).color(DIM));
            if !skipped.is_empty() {
                ui.label(
                    egui::RichText::new(format!(
                        "{} tracks skipped: their output carries no channel",
                        skipped.len()
                    ))
                    .small()
                    .color(AMBER),
                );
            }
        });

        if let Some(report) = &self.snap_form.report {
            ui.add_space(6.0);
            match report {
                Ok(msg) => ui.label(egui::RichText::new(msg.clone()).color(GREEN)),
                Err(e) => ui.label(egui::RichText::new(format!("error: {e}")).color(RED)),
            };
        }

        ui.add_space(10.0);
        theme::titled_card(ui, "NAMES TO BE WRITTEN", |ui| {
            if entries.is_empty() {
                theme::empty(ui, "Read a session first.");
                return;
            }
            egui::Grid::new("snap_preview").num_columns(4).striped(true).spacing([12.0, 4.0]).show(
                ui,
                |ui| {
                    theme::column(ui, "OUT");
                    theme::column(ui, "CH");
                    theme::column(ui, "TRACK");
                    theme::column(ui, "CONSOLE NAME");
                    ui.end_row();
                    for e in &entries {
                        theme::num(ui, e.output.map(|o| o.to_string()).unwrap_or_else(|| "-".into()));
                        theme::num(ui, e.channel.to_string());
                        cell(ui, 200.0, egui::RichText::new(&e.track).color(DIM));
                        let truncated = e.name != e.track;
                        cell(
                            ui,
                            160.0,
                            egui::RichText::new(&e.name).color(if truncated { AMBER } else { TEXT }),
                        );
                        ui.end_row();
                    }
                },
            );
        });
    }

    fn read_session_tracks(&mut self) {
        let path = PathBuf::from(self.snap_form.session.trim());
        match crate::markers::resolve_session_path(&path)
            .and_then(|p| snapshot::read_tracks(&p, self.snap_form.include_busses))
        {
            Ok(tracks) => {
                self.snap_form.report = Some(Ok(format!("read {} tracks from the session", tracks.len())));
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
        self.snap_form.report =
            match snapshot::write(&plan, &req, &self.cfg.snapshot, &self.cfg.wing, &out) {
                Ok(report) => {
                    let mut msg = match &report.template {
                        Some(t) => format!(
                            "wrote {} names into a copy of {} at {}",
                            report.written,
                            t.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                            report.path.display()
                        ),
                        None => format!("wrote {} channels to {}", report.written, report.path.display()),
                    };
                    if !report.unmatched.is_empty() {
                        msg.push_str(&format!(" - no entry for channels {:?}", report.unmatched));
                    }
                    Some(Ok(msg))
                }
                Err(e) => Some(Err(format!("{e:#}"))),
            };
    }

    fn log_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Clear").clicked() {
                self.log.clear();
            }
            ui.label(egui::RichText::new("newest at the bottom").small().color(DIM));
        });
        ui.add_space(6.0);
        let lines = self.log.lines();
        theme::card(ui, |ui| {
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
                            DIM
                        };
                        ui.label(egui::RichText::new(line).monospace().size(11.5).color(color));
                    }
                });
        });
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        theme::titled_card(ui, "SYNC", |ui| {
            let mut names_enabled = self.snap.names_enabled;
            if ui.checkbox(&mut names_enabled, "Channel name sync").changed() {
                self.send(Command::SetNamesEnabled(names_enabled));
            }
            ui.horizontal(|ui| {
                theme::field(ui, "Direction");
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
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "CONNECTIONS", |ui| {
            ui.label(
                egui::RichText::new("Hosts, ports and addresses live in the config file; edit it and restart.")
                    .small()
                    .color(DIM),
            );
            ui.add_space(4.0);
            for (label, value) in [
                ("Console", format!("{}:{}", self.cfg.wing.host, self.cfg.wing.port)),
                ("LiveTrax", format!("{}:{}", self.cfg.livetrax.host, self.cfg.livetrax.port)),
                ("Channel name address", self.cfg.wing.name_address.clone()),
                ("Scene address", self.cfg.scenes.scene_address.clone()),
            ] {
                ui.horizontal(|ui| {
                    theme::field(ui, label);
                    ui.label(egui::RichText::new(value).monospace().color(TEXT));
                });
            }
            ui.horizontal(|ui| {
                theme::field(ui, "Session file");
                ui.label(
                    egui::RichText::new(
                        self.snap
                            .session_file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "-".into()),
                    )
                    .monospace()
                    .color(TEXT),
                );
                if ui.small_button("Choose").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("LiveTrax session", &["ardour"])
                        .pick_file()
                    {
                        self.send(Command::SetSessionFile(file));
                    }
                }
            });
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "CONFIG FILE", |ui| {
            ui.horizontal(|ui| {
                if ui.button("Save settings to the config file").clicked() {
                    self.send(Command::SaveConfig(self.cfg_path.clone()));
                }
                ui.label(
                    egui::RichText::new("Rewrites the TOML from what is running; comments are lost.")
                        .small()
                        .color(DIM),
                );
            });
        });
    }
}

// ---------------------------------------------------------- helpers --------

fn bar_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::PANEL)
        .inner_margin(egui::Margin::symmetric(14, 8))
}

fn tabs_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::BG)
        .inner_margin(egui::Margin { left: 12, right: 12, top: 6, bottom: 4 })
}

/// A fixed-width, left-aligned table cell, so columns stay put as names change.
fn cell(ui: &mut egui::Ui, width: f32, text: egui::RichText) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 18.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add(egui::Label::new(text).truncate());
        },
    );
}

/// A small rounded label for a piece of live state.
fn chip(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::CARD)
        .corner_radius(egui::CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().color(ACCENT));
        });
}

fn link_color(last: Option<std::time::Instant>) -> egui::Color32 {
    match last {
        Some(t) if t.elapsed() < Duration::from_secs(10) => GREEN,
        Some(_) => AMBER,
        None => RED,
    }
}

fn age(last: Option<std::time::Instant>) -> String {
    match last {
        Some(t) => format!("{:.0}s", t.elapsed().as_secs_f32()),
        None => "silent".to_string(),
    }
}

fn timecode(samples: i64, rate: f64) -> String {
    let rate = if rate > 1.0 { rate } else { 48_000.0 };
    let total = samples.max(0) as f64 / rate;
    let h = (total / 3600.0).floor() as u64;
    let m = ((total % 3600.0) / 60.0).floor() as u64;
    let s = total % 60.0;
    format!("{h:02}:{m:02}:{s:06.3}")
}
