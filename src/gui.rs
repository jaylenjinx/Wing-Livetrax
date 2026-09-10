//! Desktop front end.
//!
//! The GUI owns no state of its own beyond form fields: it renders the latest
//! snapshot published by the bridge and sends commands back over a channel.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{Action, Config, PatchSource};
use crate::session::{self, SessionRequest};
use crate::patch::OutputGroup;
use crate::patchbuild;
use crate::sheet;
use crate::prefs::Prefs;
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
        // "preferences" opens the window rather than switching tabs, and
        // "preferences/transport" opens it on a section.
        let (head, section) = tab.split_once('/').unwrap_or((tab, ""));
        if head.eq_ignore_ascii_case("preferences") || head.eq_ignore_ascii_case("prefs") {
            let config = app.cfg.clone();
            app.prefs.open_at(&config, section);
        } else {
            app.tab = Tab::from_name(tab).with_context(|| format!("unknown tab {tab:?}"))?;
        }
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
    PatchSheet,
    Log,
}

impl Tab {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_lowercase().replace(['-', '_'], "").as_str() {
            "channels" => Tab::Channels,
            "transport" => Tab::Transport,
            "scenes" => Tab::Scenes,
            "newsession" => Tab::NewSession,
            "snapshot" => Tab::Snapshot,
            "patchsheet" | "sheet" => Tab::PatchSheet,
            "log" => Tab::Log,
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
    sheet_form: SheetForm,
    /// The "go to timecode" entry.
    tc_input: String,
    /// Where the scrub bar is being dragged to, while it is being dragged.
    scrubbing: Option<i64>,
    prefs: Prefs,
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
            sheet_form: SheetForm::new(&cfg),
            tc_input: String::new(),
            scrubbing: None,
            prefs: Prefs::new(&cfg),
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

        // Cmd-, is the preferences shortcut everywhere else on this platform.
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.prefs.open_with(&self.cfg);
        }
        for command in self.prefs.show(ui.ctx(), &self.cfg_path, &self.snap.console_events) {
            if let Command::ApplyConfig(config) = &command {
                self.cfg = (**config).clone();
            }
            self.send(command);
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
                        Tab::PatchSheet => self.sheet_tab(ui),
                        Tab::Log => self.log_tab(ui),
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
                egui::RichText::new(
                    self.snap
                        .timecode
                        .clone()
                        .unwrap_or_else(|| timecode(self.snap.position, self.snap.sample_rate)),
                )
                .monospace()
                .size(16.0)
                .color(if rolling { GREEN } else { TEXT }),
            );
            if !self.snap.fps.is_empty() {
                ui.label(egui::RichText::new(format!("{} fps", self.snap.fps)).small().color(DIM));
            }

            if let Some(marker) = self.snap.current_marker.clone() {
                chip(ui, &format!("marker  {marker}"));
            }
            if let Some(scene) = self.snap.current_scene {
                chip(ui, &format!("scene  {scene}"));
            }
            if self.snap.looping {
                chip(ui, "loop");
            }
            if self.snap.punch_in || self.snap.punch_out {
                chip(ui, "punch");
            }
            if self.snap.click {
                chip(ui, "click");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Preferences").clicked() {
                    self.prefs.open_with(&self.cfg);
                }
            });
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
                (Tab::PatchSheet, "Patch sheet"),
                (Tab::Log, "Log"),
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
        self.playback(ui);
        ui.add_space(10.0);

        theme::titled_card(ui, "GO TO TIMECODE", |ui| {
            ui.horizontal(|ui| {
                let entry = ui.add(
                    egui::TextEdit::singleline(&mut self.tc_input)
                        .hint_text("01:02:03:04")
                        .font(egui::TextStyle::Monospace)
                        .desired_width(140.0),
                );
                let submitted = entry.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let parsed = crate::timecode::Timecode::parse(&self.tc_input);
                let go = ui.add_enabled(parsed.is_some(), theme::primary("Go")).clicked();
                if (go || submitted) && parsed.is_some() {
                    self.send(Command::LocateTimecode(self.tc_input.clone()));
                }
                if ui.button("From playhead").clicked() {
                    self.tc_input = self
                        .snap
                        .timecode
                        .clone()
                        .unwrap_or_default()
                        .replace(';', ":");
                }
                if !self.tc_input.trim().is_empty() && parsed.is_none() {
                    ui.label(
                        egui::RichText::new("hours:minutes:seconds:frames").small().color(AMBER),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(format!(
                            "at {} fps, {} Hz",
                            self.snap.fps, self.snap.sample_rate
                        ))
                        .small()
                        .color(DIM),
                    );
                }
            });
        });
        ui.add_space(10.0);
        self.cue_log(ui);
    }

    /// The playback surface: the things you reach for while a take is running.
    fn playback(&mut self, ui: &mut egui::Ui) {
        theme::titled_card(ui, "PLAYBACK", |ui| {
            let rolling = self.snap.playing;

            ui.horizontal(|ui| {
                if ui.button("|< Start").clicked() {
                    self.send(Command::Transport(Action::GotoStart));
                }
                if ui.button("<< Back").clicked() {
                    self.send(Command::Transport(Action::Rewind));
                }
                if ui.add(theme::primary(if rolling { "Stop" } else { "Play" })).clicked() {
                    self.send(Command::Transport(if rolling {
                        Action::Stop
                    } else {
                        Action::Play
                    }));
                }
                if ui.button("Forward >>").clicked() {
                    self.send(Command::Transport(Action::FastForward));
                }
                if ui.button("End >|").clicked() {
                    self.send(Command::Transport(Action::GotoEnd));
                }
                ui.add_space(8.0);
                theme::dot(ui, if self.snap.recording { RED } else { theme::LINE });
                if ui.button("Rec arm").clicked() {
                    self.send(Command::Transport(Action::RecordArmToggle));
                }
                if ui.button("Record + roll").clicked() {
                    self.send(Command::Transport(Action::RecordStart));
                }
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (label, active, action) in [
                    ("Loop", self.snap.looping, Action::LoopToggle),
                    ("Punch in", self.snap.punch_in, Action::PunchIn),
                    ("Punch out", self.snap.punch_out, Action::PunchOut),
                    ("Click", self.snap.click, Action::ClickToggle),
                ] {
                    let text = egui::RichText::new(label).color(if active { TEXT } else { DIM });
                    if ui.selectable_label(active, text).clicked() {
                        self.send(Command::Transport(action));
                    }
                }
                ui.separator();
                if ui.button("Arm every track").clicked() {
                    self.send(Command::Transport(Action::AllRecEnable));
                }
                if ui.button("Drop marker").clicked() {
                    self.send(Command::Transport(Action::AddMarker));
                }
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                theme::field(ui, "Nudge");
                for (label, action) in [
                    ("-10 s", Action::JumpSeconds(-10.0)),
                    ("-1 s", Action::JumpSeconds(-1.0)),
                    ("+1 s", Action::JumpSeconds(1.0)),
                    ("+10 s", Action::JumpSeconds(10.0)),
                ] {
                    if ui.button(label).clicked() {
                        self.send(Command::Transport(action));
                    }
                }
                ui.separator();
                for (label, action) in [
                    ("-1 bar", Action::JumpBars(-1.0)),
                    ("+1 bar", Action::JumpBars(1.0)),
                ] {
                    if ui.button(label).clicked() {
                        self.send(Command::Transport(action));
                    }
                }
                ui.separator();
                if ui.button("Previous marker").clicked() {
                    self.send(Command::Transport(Action::PrevMarker));
                }
                if ui.button("Next marker").clicked() {
                    self.send(Command::Transport(Action::NextMarker));
                }
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                theme::field(ui, "Speed");
                let mut speed = self.snap.speed;
                ui.spacing_mut().slider_width = 220.0;
                let slider = ui.add(
                    egui::Slider::new(&mut speed, -2.0..=2.0)
                        .fixed_decimals(2)
                        .suffix("x")
                        .clamping(egui::SliderClamping::Always),
                );
                // Send on release: dragging would flood the DAW with speeds.
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    self.send(Command::Transport(Action::SetSpeed(speed)));
                }
                for (label, value) in [("0.5x", 0.5), ("1x", 1.0), ("2x", 2.0)] {
                    if ui.small_button(label).clicked() {
                        self.send(Command::Transport(Action::SetSpeed(value)));
                    }
                }
            });

            ui.add_space(8.0);
            self.scrub_bar(ui);
        });
    }

    /// Drag anywhere in the session. The playhead follows on release, not
    /// during the drag - locating on every frame would stutter the transport.
    fn scrub_bar(&mut self, ui: &mut egui::Ui) {
        let end = self.snap.session_end.max(self.snap.position).max(1);
        let mut value = self.scrubbing.unwrap_or(self.snap.position).clamp(0, end);
        ui.horizontal(|ui| {
            theme::num(ui, timecode_of(&self.snap, value));
            // Sliders take their width from the spacing, not from add_sized.
            ui.spacing_mut().slider_width = (ui.available_width() - 130.0).max(160.0);
            let response = ui.add(egui::Slider::new(&mut value, 0..=end).show_value(false));
            if response.dragged() || response.drag_started() {
                self.scrubbing = Some(value);
            }
            if response.drag_stopped() {
                self.send(Command::LocateSamples(value));
                self.scrubbing = None;
            }
            // A click without a drag still moves the playhead.
            if response.changed() && !response.dragged() && self.scrubbing.is_none() {
                self.send(Command::LocateSamples(value));
            }
            theme::num(ui, timecode_of(&self.snap, end));
        });
    }

    /// The show log: what happened, and at what timecode.
    fn cue_log(&mut self, ui: &mut egui::Ui) {
        theme::titled_card(ui, &format!("SHOW LOG ({})", self.snap.cues.len()), |ui| {
            ui.horizontal(|ui| {
                if ui.button("Export CSV").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name("show-log.csv")
                        .add_filter("CSV", &["csv"])
                        .save_file()
                    {
                        self.send(Command::ExportCues(path));
                    }
                }
                if ui.button("Clear").clicked() {
                    self.send(Command::ClearCues);
                }
                ui.label(
                    egui::RichText::new("markers, scene recalls and takes, stamped with timecode")
                        .small()
                        .color(DIM),
                );
            });
            ui.add_space(4.0);
            if self.snap.cues.is_empty() {
                theme::empty(ui, "Nothing logged yet - drop a marker or recall a scene.");
                return;
            }
            egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(
                ui,
                |ui| {
                    egui::Grid::new("cues").num_columns(3).striped(true).spacing([12.0, 4.0]).show(
                        ui,
                        |ui| {
                            theme::column(ui, "TIMECODE");
                            theme::column(ui, "WHAT");
                            theme::column(ui, "DETAIL");
                            ui.end_row();
                            for cue in self.snap.cues.iter().rev() {
                                theme::num(ui, cue.timecode.clone());
                                ui.label(
                                    egui::RichText::new(cue.kind.label()).small().color(
                                        match cue.kind {
                                            crate::shared::CueKind::TakeStart => GREEN,
                                            crate::shared::CueKind::TakeStop => DIM,
                                            crate::shared::CueKind::Scene => ACCENT,
                                            crate::shared::CueKind::Marker => TEXT,
                                        },
                                    ),
                                );
                                cell(ui, 260.0, egui::RichText::new(&cue.detail).color(TEXT));
                                ui.end_row();
                            }
                        },
                    );
                },
            );
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
                    styles: Vec::new(),
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

/// A sample position on the same clock the header shows.
fn timecode_of(snap: &Snapshot, samples: i64) -> String {
    crate::timecode::from_samples(
        samples,
        snap.sample_rate,
        snap.fps_value,
        snap.tc_offset_frames,
    )
    .to_string()
}

fn timecode(samples: i64, rate: f64) -> String {
    let rate = if rate > 1.0 { rate } else { 48_000.0 };
    let total = samples.max(0) as f64 / rate;
    let h = (total / 3600.0).floor() as u64;
    let m = ((total % 3600.0) / 60.0).floor() as u64;
    let s = total % 60.0;
    format!("{h:02}:{m:02}:{s:06.3}")
}

// ------------------------------------------------------------ patch sheet ---

/// The patch-sheet tab's state. The build itself needs neither the console nor
/// the DAW, so unlike the other tabs this one runs on the spot rather than
/// handing a command to the bridge thread.
struct SheetForm {
    sheet: String,
    /// The desk the sheet is for. A Qu builds the session only.
    desk: patchbuild::Desk,
    base: String,
    group: String,
    dest: String,
    name: String,
    template: String,
    templates: Vec<PathBuf>,
    sample_rate: u32,
    make_session: bool,
    connect_inputs: bool,
    arm: bool,
    label_sources: bool,
    keep_unlisted: bool,
    /// The sheet as last read, or why it could not be.
    read: Option<Result<sheet::Sheet, String>>,
    /// What the last Build or Preview did.
    result: Option<Result<Vec<String>, String>>,
}

impl SheetForm {
    fn new(cfg: &Config) -> Self {
        let templates = session::discover_templates();
        Self {
            sheet: String::new(),
            desk: patchbuild::Desk::default(),
            base: String::new(),
            group: cfg.patch.output_group.clone(),
            dest: cfg
                .livetrax
                .session_file
                .as_ref()
                .and_then(|p| p.parent().and_then(|d| d.parent()))
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            name: String::new(),
            template: templates.first().map(|p| p.display().to_string()).unwrap_or_default(),
            templates,
            sample_rate: 48_000,
            make_session: true,
            connect_inputs: true,
            arm: true,
            label_sources: true,
            keep_unlisted: false,
            read: None,
            result: None,
        }
    }

    fn request(&self) -> patchbuild::BuildRequest {
        let base = self.base.trim();
        patchbuild::BuildRequest {
            desk: self.desk,
            base: (!base.is_empty()).then(|| PathBuf::from(base)),
            record_group: self.group.trim().to_uppercase(),
            label_sources: self.label_sources,
            keep_unlisted_outputs: self.keep_unlisted,
        }
    }
}

impl App {
    fn sheet_tab(&mut self, ui: &mut egui::Ui) {
        theme::titled_card(ui, "THE SHEET", |ui| {
            ui.label(
                egui::RichText::new(
                    "One row per channel: the name, the socket it arrives on, gain, phantom, \
                     colour, DCA, and which track records it. Both ends of the show are built \
                     from it, so the console and the DAW cannot disagree.",
                )
                .small()
                .color(DIM),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                theme::field(ui, "Desk");
                for desk in patchbuild::Desk::ALL {
                    let picked = self.sheet_form.desk == desk;
                    let label = egui::RichText::new(desk.label())
                        .color(if picked { TEXT } else { DIM });
                    if ui.selectable_label(picked, label).clicked() {
                        self.sheet_form.desk = desk;
                    }
                }
            });
            if !self.sheet_form.desk.writes_snapshot() {
                ui.horizontal(|ui| {
                    theme::field(ui, "");
                    ui.label(
                        egui::RichText::new(
                            "A Qu has no published file format, so the sheet builds the LiveTrax \
                             session only. Set the desk up from the same sheet by hand.",
                        )
                        .small()
                        .color(DIM),
                    );
                });
            }
            ui.horizontal(|ui| {
                theme::field(ui, "Patch sheet");
                ui.add(egui::TextEdit::singleline(&mut self.sheet_form.sheet).desired_width(360.0));
                if ui.button("Browse").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("Spreadsheet", &["csv", "tsv", "txt"])
                        .pick_file()
                    {
                        self.sheet_form.sheet = file.display().to_string();
                        self.read_sheet();
                    }
                }
                if ui.button("Read").clicked() {
                    self.read_sheet();
                }
                if ui.button("Write a starter sheet").clicked() {
                    self.write_sheet_template();
                }
            });
            ui.add_space(2.0);
            match &self.sheet_form.read {
                None => theme::empty(ui, "No sheet read yet. Pick one, or write a starter sheet to fill in."),
                Some(Err(e)) => {
                    ui.label(egui::RichText::new(format!("error: {e}")).color(RED));
                }
                Some(Ok(sheet)) => {
                    let tracks = sheet.rows.iter().filter(|r| r.track.is_some()).count();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} channels, {tracks} of them recorded",
                            sheet.rows.len()
                        ))
                        .color(GREEN),
                    );
                    if !sheet.unknown_columns.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                "carried through untouched: {}",
                                sheet.unknown_columns.join(", ")
                            ))
                            .small()
                            .color(DIM),
                        );
                    }
                    for w in &sheet.warnings {
                        ui.label(egui::RichText::new(format!("warning: {w}")).small().color(AMBER));
                    }
                }
            }
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "THE CONSOLE", |ui| {
            if !self.sheet_form.desk.writes_snapshot() {
                ui.label(
                    egui::RichText::new(format!(
                        "Nothing to build for {} {}: gain, phantom, colour, DCA and the rest are \
                         read from the sheet but have no file to go into. The session below is \
                         built from the same rows.",
                        self.sheet_form.desk.article(),
                        self.sheet_form.desk.label()
                    ))
                    .small()
                    .color(DIM),
                );
                return;
            }
            ui.horizontal(|ui| {
                theme::field(ui, "Base snapshot");
                ui.add(egui::TextEdit::singleline(&mut self.sheet_form.base).desired_width(360.0));
                if ui.button("Browse").clicked() {
                    if let Some(file) =
                        rfd::FileDialog::new().add_filter("WING snapshot", &["snap"]).pick_file()
                    {
                        self.sheet_form.base = file.display().to_string();
                    }
                }
                if !self.sheet_form.base.trim().is_empty() && ui.button("Clear").clicked() {
                    self.sheet_form.base.clear();
                }
            });
            ui.horizontal(|ui| {
                theme::field(ui, "");
                ui.label(
                    egui::RichText::new(if self.sheet_form.base.trim().is_empty() {
                        "empty: start from a factory console"
                    } else {
                        "the sheet is laid over this file, so its effects and busses survive"
                    })
                    .small()
                    .color(DIM),
                );
            });
            ui.horizontal(|ui| {
                theme::field(ui, "Recorded on");
                ui.add(egui::TextEdit::singleline(&mut self.sheet_form.group).desired_width(80.0));
                ui.label(
                    egui::RichText::new("the port group the DAW records - the Track column is an output of it")
                        .small()
                        .color(DIM),
                );
            });
            ui.horizontal(|ui| {
                theme::field(ui, "Options");
                ui.vertical(|ui| {
                    ui.checkbox(
                        &mut self.sheet_form.label_sources,
                        "put each name, colour and icon on its source as well",
                    );
                    ui.checkbox(
                        &mut self.sheet_form.keep_unlisted,
                        "leave the record group's other outputs as the base had them",
                    );
                });
            });
        });
        ui.add_space(10.0);

        theme::titled_card(ui, "THE SESSION", |ui| {
            if self.sheet_form.desk.writes_snapshot() {
                ui.checkbox(&mut self.sheet_form.make_session, "also build a LiveTrax session");
            } else {
                // It is the only thing being built, so there is nothing to opt out of.
                self.sheet_form.make_session = true;
            }
            ui.add_enabled_ui(self.sheet_form.make_session, |ui| {
                ui.horizontal(|ui| {
                    theme::field(ui, "Sessions folder");
                    ui.add(egui::TextEdit::singleline(&mut self.sheet_form.dest).desired_width(360.0));
                    if ui.button("Browse").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.sheet_form.dest = dir.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    theme::field(ui, "Template");
                    ui.add(egui::TextEdit::singleline(&mut self.sheet_form.template).desired_width(360.0));
                    if ui.button("Browse").clicked() {
                        if let Some(file) = rfd::FileDialog::new()
                            .add_filter("LiveTrax session", &["ardour", "template"])
                            .pick_file()
                        {
                            self.sheet_form.template = file.display().to_string();
                        }
                    }
                    // The track graph comes from a session your own LiveTrax
                    // wrote, so the installed templates are the safe choices.
                    if !self.sheet_form.templates.is_empty() {
                        egui::ComboBox::from_id_salt("sheet_template")
                            .selected_text("Installed")
                            .show_ui(ui, |ui| {
                                let found = self.sheet_form.templates.clone();
                                for path in found {
                                    let label = path
                                        .file_stem()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| path.display().to_string());
                                    if ui.selectable_label(false, label).clicked() {
                                        self.sheet_form.template = path.display().to_string();
                                    }
                                }
                            });
                    }
                });
                ui.horizontal(|ui| {
                    theme::field(ui, "Sample rate");
                    egui::ComboBox::from_id_salt("sheet_rate")
                        .selected_text(format!("{} Hz", self.sheet_form.sample_rate))
                        .show_ui(ui, |ui| {
                            for rate in [44_100, 48_000, 88_200, 96_000] {
                                ui.selectable_value(
                                    &mut self.sheet_form.sample_rate,
                                    rate,
                                    format!("{rate} Hz"),
                                );
                            }
                        });
                    ui.checkbox(&mut self.sheet_form.connect_inputs, "connect inputs");
                    ui.checkbox(&mut self.sheet_form.arm, "arm the recorded tracks");
                });
            });
        });
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            theme::field(ui, "Show name");
            ui.add(egui::TextEdit::singleline(&mut self.sheet_form.name).desired_width(240.0));
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ready = matches!(self.sheet_form.read, Some(Ok(_)))
                && !self.sheet_form.name.trim().is_empty();
            if ui.add_enabled(ready, theme::primary("Build")).clicked() {
                self.build_from_sheet(false);
            }
            if ui
                .add_enabled(matches!(self.sheet_form.read, Some(Ok(_))), egui::Button::new("Preview"))
                .clicked()
            {
                self.build_from_sheet(true);
            }
            if !ready {
                ui.label(
                    egui::RichText::new("read a sheet and give the show a name")
                        .small()
                        .color(DIM),
                );
            }
        });

        if let Some(result) = &self.sheet_form.result {
            ui.add_space(6.0);
            theme::card(ui, |ui| match result {
                Ok(lines) => {
                    for line in lines {
                        let colour = match line.split_once(": ") {
                            Some(("warning", _)) => AMBER,
                            _ if line.starts_with("  ") => DIM,
                            _ => TEXT,
                        };
                        ui.label(egui::RichText::new(line).small().color(colour));
                    }
                }
                Err(e) => {
                    ui.label(egui::RichText::new(format!("error: {e}")).color(RED));
                }
            });
        }
    }

    fn read_sheet(&mut self) {
        let path = self.sheet_form.sheet.trim().to_string();
        self.sheet_form.result = None;
        if path.is_empty() {
            self.sheet_form.read = None;
            return;
        }
        self.sheet_form.read =
            Some(sheet::read(std::path::Path::new(&path)).map_err(|e| format!("{e:#}")));
        // A sheet usually sits beside the show it is for, and the show usually
        // shares its name - a good enough guess to save typing.
        if self.sheet_form.name.trim().is_empty() {
            if let Some(stem) = std::path::Path::new(&path).file_stem() {
                self.sheet_form.name = stem.to_string_lossy().into_owned();
            }
        }
    }

    fn write_sheet_template(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Spreadsheet", &["csv"])
            .set_file_name("patch-sheet.csv")
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, sheet::template()) {
            Ok(()) => {
                self.sheet_form.sheet = path.display().to_string();
                self.read_sheet();
            }
            Err(e) => self.sheet_form.result = Some(Err(format!("writing {}: {e}", path.display()))),
        }
    }

    fn build_from_sheet(&mut self, preview: bool) {
        let Some(Ok(sheet)) = &self.sheet_form.read else { return };
        let built = match patchbuild::build(sheet, &self.sheet_form.request()) {
            Ok(b) => b,
            Err(e) => {
                self.sheet_form.result = Some(Err(format!("{e:#}")));
                return;
            }
        };
        let mut lines = vec![if built.snapshot.is_some() {
            format!(
                "{} channels, {} tracks on {}, {} nodes moved",
                built.report.channels.len(),
                built.report.tracks.len(),
                self.sheet_form.group.trim().to_uppercase(),
                built.report.changes.len()
            )
        } else {
            format!(
                "{} channels, {} tracks for {} {}",
                built.report.channels.len(),
                built.report.tracks.len(),
                self.sheet_form.desk.article(),
                self.sheet_form.desk.label()
            )
        }];
        if built.report.cleared > 0 {
            lines.push(format!(
                "{} outputs the sheet does not use were switched off",
                built.report.cleared
            ));
        }
        if preview {
            for change in built.report.changes.iter().take(400) {
                lines.push(format!("  {change}"));
            }
            if built.report.changes.len() > 400 {
                lines.push(format!("  ... and {} more", built.report.changes.len() - 400));
            }
            for w in &built.report.warnings {
                lines.push(format!("warning: {w}"));
            }
            lines.push("(preview - nothing was written)".into());
            self.sheet_form.result = Some(Ok(lines));
            return;
        }

        let name = self.sheet_form.name.trim().to_string();
        let mut folder: Option<PathBuf> = None;
        if self.sheet_form.make_session {
            let template = self.sheet_form.template.trim();
            let req = SessionRequest {
                parent_dir: PathBuf::from(self.sheet_form.dest.trim()),
                name: name.clone(),
                sample_rate: self.sheet_form.sample_rate,
                tracks: patchbuild::daw_names(&built.report.tracks),
                template: (!template.is_empty()).then(|| PathBuf::from(template)),
                connect_inputs: self.sheet_form.connect_inputs,
                allow_minimal: false,
                styles: built
                    .report
                    .tracks
                    .iter()
                    .map(|t| session::TrackStyle {
                        colour: t.colour.and_then(patchbuild::track_colour),
                        rec_arm: self.sheet_form.arm && t.channel.is_some(),
                    })
                    .collect(),
            };
            match session::create(&req) {
                Ok(r) => {
                    lines.push(format!("created {} with {} tracks", r.session_file.display(), r.tracks));
                    for w in &r.warnings {
                        lines.push(format!("warning: {w}"));
                    }
                    folder = Some(r.folder);
                }
                Err(e) => {
                    self.sheet_form.result = Some(Err(format!("{e:#}")));
                    return;
                }
            }
        }
        if built.snapshot.is_none() {
            for w in &built.report.warnings {
                lines.push(format!("warning: {w}"));
            }
            self.sheet_form.result = Some(Ok(lines));
            return;
        }

        // With no session to put it in, the snapshot goes beside the sheet it
        // was built from, where the person who filled the sheet in will look.
        let snap = match &folder {
            Some(f) => f.join(format!("{name}.snap")),
            None => std::path::Path::new(self.sheet_form.sheet.trim())
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.join(format!("{name}.snap")))
                .unwrap_or_else(|| PathBuf::from(format!("{name}.snap"))),
        };
        if let Err(e) = built.write_snap(&snap) {
            self.sheet_form.result = Some(Err(format!("{e:#}")));
            return;
        }
        lines.push(format!("wrote {}", snap.display()));
        for w in &built.report.warnings {
            lines.push(format!("warning: {w}"));
        }
        self.sheet_form.result = Some(Ok(lines));
    }
}
