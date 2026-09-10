//! The preferences window: everything the TOML file holds, editable in place.
//!
//! Edits go into a draft copy. **Apply** hands the draft to the bridge, which
//! takes on everything it can without restarting; **Save** also writes the file.
//! Nothing is touched until one of those, so a half-typed address never reaches
//! the console.

use std::path::Path;

use crate::config::{
    Action, Arg, ButtonMap, Config, Direction, LedMap, LedSource, Pair, PatchSource, RecArmMap,
    SceneDirection, SceneMarker,
};
use crate::shared::{Command, ConsoleActivity};
use crate::theme::{self, ACCENT, AMBER, DIM, TEXT};
use crate::timecode::Fps;

/// Ardour's `/set_surface` strip-type bits.
const STRIP_TYPES: [(u32, &str); 11] = [
    (1, "audio tracks"), (2, "midi tracks"), (4, "audio busses"), (8, "midi busses"),
    (16, "VCAs"), (32, "master"), (64, "monitor"), (128, "foldback busses"),
    (256, "selected only"), (512, "hidden"), (1024, "use groups"),
];

/// Ardour's `/set_surface` feedback bits.
const FEEDBACK_BITS: [(u32, &str); 15] = [
    (1, "button status"), (2, "variable controls"), (4, "ssid in the path"), (8, "heartbeat"),
    (16, "master section"), (32, "bar and beat"), (64, "timecode"), (128, "meter as dB"),
    (256, "meter as 16 bit"), (512, "signal present"), (1024, "playhead in samples"),
    (2048, "playhead as min:sec"), (4096, "playhead as a clock"), (8192, "select feedback"),
    (16384, "OSC 1.0 replies"),
];

#[derive(PartialEq, Eq, Clone, Copy)]
enum Section {
    Console,
    LiveTrax,
    Names,
    Patch,
    Transport,
    Scenes,
    Timecode,
    Snapshot,
    Mapping,
}

impl Section {
    fn from_name(name: &str) -> Option<Section> {
        Section::ALL
            .iter()
            .find(|(_, label)| label.eq_ignore_ascii_case(name))
            .map(|(section, _)| *section)
            .or_else(|| match name.to_lowercase().as_str() {
                "console" | "wing" => Some(Section::Console),
                "livetrax" | "daw" => Some(Section::LiveTrax),
                "names" => Some(Section::Names),
                "patch" | "output" => Some(Section::Patch),
                "transport" | "buttons" => Some(Section::Transport),
                "scenes" => Some(Section::Scenes),
                "timecode" => Some(Section::Timecode),
                "snapshot" | "snapshots" => Some(Section::Snapshot),
                "map" | "mapping" => Some(Section::Mapping),
                _ => None,
            })
    }

    const ALL: [(Section, &'static str); 9] = [
        (Section::Console, "Console"),
        (Section::LiveTrax, "LiveTrax"),
        (Section::Names, "Names"),
        (Section::Patch, "Recorded output"),
        (Section::Transport, "Transport"),
        (Section::Scenes, "Scenes"),
        (Section::Timecode, "Timecode"),
        (Section::Snapshot, "Snapshots"),
        (Section::Mapping, "Manual map"),
    ];
}

pub struct Prefs {
    pub open: bool,
    draft: Config,
    section: Section,
    status: Option<String>,
    /// Which button row is waiting for a press on the console.
    learning: Option<usize>,
    /// Console messages older than this were already there when we armed.
    learn_from: u64,
}

impl Prefs {
    pub fn new(cfg: &Config) -> Self {
        Self {
            open: false,
            draft: cfg.clone(),
            section: Section::Console,
            status: None,
            learning: None,
            learn_from: 0,
        }
    }

    /// Open on a named section, for `gui --tab preferences/transport`.
    pub fn open_at(&mut self, cfg: &Config, section: &str) {
        self.open_with(cfg);
        if let Some(section) = Section::from_name(section) {
            self.section = section;
        }
    }

    /// Open on the configuration as it currently stands.
    pub fn open_with(&mut self, cfg: &Config) {
        self.draft = cfg.clone();
        self.status = None;
        self.learning = None;
        self.open = true;
    }

    /// Draw the window. Returns whatever the buttons asked for.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        cfg_path: &Path,
        console: &[ConsoleActivity],
    ) -> Vec<Command> {
        let mut commands = Vec::new();
        if !self.open {
            return commands;
        }
        self.take_learned_press(console);
        let mut open = true;
        egui::Window::new("Preferences")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([760.0, 560.0])
            .min_width(620.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(14))
                    .stroke(egui::Stroke::new(1.0, theme::LINE)),
            )
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    self.section_list(ui);
                    // A bare separator in a horizontal layout stretches to the
                    // available height and drags the window with it.
                    ui.add_sized([8.0, 400.0], egui::Separator::default().vertical());
                    ui.vertical(|ui| {
                        // A fixed height keeps the window at its own size, with
                        // the buttons below always in reach.
                        egui::ScrollArea::vertical()
                            .auto_shrink([false; 2])
                            .max_height(380.0)
                            .show(ui, |ui| {
                                ui.set_min_width(520.0);
                                match self.section {
                                    Section::Console => self.console(ui),
                                    Section::LiveTrax => self.livetrax(ui),
                                    Section::Names => self.names(ui),
                                    Section::Patch => self.patch(ui),
                                    Section::Transport => self.transport(ui, console),
                                    Section::Scenes => self.scenes(ui),
                                    Section::Timecode => self.timecode(ui),
                                    Section::Snapshot => self.snapshot(ui),
                                    Section::Mapping => self.mapping(ui),
                                }
                            });
                    });
                });
                ui.separator();
                self.footer(ui, cfg_path, &mut commands);
            });
        self.open = open;
        commands
    }

    fn section_list(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.set_width(150.0);
            for (section, label) in Section::ALL {
                let selected = self.section == section;
                let text = egui::RichText::new(label).color(if selected { TEXT } else { DIM });
                if ui.selectable_label(selected, text).clicked() {
                    self.section = section;
                }
            }
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, cfg_path: &Path, commands: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            if ui.add(theme::primary("Apply")).clicked() {
                commands.push(Command::ApplyConfig(Box::new(self.draft.clone())));
                self.status = Some("Applied to the running bridge.".into());
            }
            if ui.button("Save to file").clicked() {
                commands.push(Command::ApplyConfig(Box::new(self.draft.clone())));
                commands.push(Command::SaveConfig(cfg_path.to_path_buf()));
                self.status = Some(format!("Applied and written to {}", cfg_path.display()));
            }
            if ui.button("Close").clicked() {
                self.open = false;
            }
            if let Some(status) = &self.status {
                ui.label(egui::RichText::new(status).small().color(ACCENT));
            } else {
                ui.label(
                    egui::RichText::new("Hosts and ports take effect on restart.")
                        .small()
                        .color(DIM),
                );
            }
        });
    }

    // ------------------------------------------------------------ sections --

    fn console(&mut self, ui: &mut egui::Ui) {
        let wing = &mut self.draft.wing;
        heading(ui, "Console", "Where the WING is, and how its channels are addressed.");
        text_row(ui, "Host", &mut wing.host);
        num_row(ui, "OSC port", &mut wing.port, 1..=65535);
        num_row(ui, "Local port", &mut wing.local_port, 0..=65535);
        hint(ui, "0 lets the system pick. Fix it if you have firewall rules.");
        num_row(ui, "Channels", &mut wing.channels, 1..=96);
        text_row(ui, "Name address", &mut wing.name_address);
        hint(ui, "{ch} is the channel number. Confirm yours with `learn`.");
        num_row(ui, "Subscribe every (ms)", &mut wing.subscribe_interval_ms, 200..=60_000);
        num_row(ui, "Re-read names every (ms)", &mut wing.name_poll_interval_ms, 0..=600_000);
        hint(ui, "0 turns polling off and relies on the subscription alone.");
        check_row(ui, "Read values with an empty message", &mut wing.query_with_empty_args);
        string_list(ui, "Subscription messages", &mut wing.subscribe, "/*S");
    }

    fn livetrax(&mut self, ui: &mut egui::Ui) {
        heading(ui, "LiveTrax", "The DAW end, and the session the bridge follows.");
        let daw = &mut self.draft.livetrax;
        text_row(ui, "Host", &mut daw.host);
        num_row(ui, "OSC port", &mut daw.port, 1..=65535);
        num_row(ui, "Local port", &mut daw.local_port, 1..=65535);
        num_row(ui, "Re-announce every (ms)", &mut daw.refresh_interval_ms, 1_000..=600_000);
        text_row(ui, "Rename address", &mut daw.rename_address);
        text_row(ui, "Add marker address", &mut daw.add_marker_address);
        check_row(ui, "Add marker takes a name", &mut daw.add_marker_takes_name);

        ui.add_space(6.0);
        path_row(ui, "Session file", &mut daw.session_file, &["ardour"]);
        check_row(ui, "Watch the session file", &mut daw.watch_session_file);
        num_row_f64(ui, "Fallback sample rate", &mut daw.sample_rate, 8_000.0..=192_000.0);

        ui.add_space(8.0);
        bitmask(ui, "Strips to control", &mut daw.strip_types, &STRIP_TYPES);
        ui.add_space(6.0);
        bitmask(ui, "Feedback wanted", &mut daw.feedback, &FEEDBACK_BITS);
        num_row(ui, "Bank size", &mut daw.bank_size, 0..=256);
        hint(ui, "0 puts every strip in one bank.");
    }

    fn names(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Names", "Which end wins, and what gets left alone.");
        let names = &mut self.draft.names;
        check_row(ui, "Sync channel names", &mut names.enabled);
        ui.horizontal(|ui| {
            theme::field(ui, "Direction");
            for (value, label) in [
                (Direction::WingToDaw, "console -> DAW"),
                (Direction::DawToWing, "DAW -> console"),
                (Direction::Bidirectional, "both"),
            ] {
                ui.selectable_value(&mut names.direction, value, label);
            }
        });
        num_row(ui, "Settle for (ms)", &mut names.debounce_ms, 0..=5_000);
        hint(ui, "How long a name must stop changing before it is pushed.");
        num_row_usize(ui, "Length on the console", &mut names.max_len_wing, 0..=32);
        check_row(ui, "Never write an empty name", &mut names.skip_empty);
        text_row(ui, "Prefix in the DAW", &mut names.daw_prefix);
        text_row(ui, "Suffix in the DAW", &mut names.daw_suffix);
        string_list(ui, "Treated as unnamed", &mut names.ignore_prefixes, "Audio ");
    }

    fn patch(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Recorded output", "Which console output the DAW records, and where that is read from.");
        let patch = &mut self.draft.patch;
        ui.horizontal(|ui| {
            theme::field(ui, "Read the patch");
            ui.selectable_value(&mut patch.source, PatchSource::Snap, "from a .snap");
            ui.selectable_value(&mut patch.source, PatchSource::Console, "from the console");
        });
        path_row(ui, "Snapshot file", &mut patch.snap_file, &["snap"]);
        text_row(ui, "Output group", &mut patch.output_group);
        check_row(ui, "Use it for the channel map", &mut patch.use_for_map);
        check_row(ui, "Ask the console at startup", &mut patch.query_on_start);

        ui.add_space(10.0);
        ui.label(egui::RichText::new("ADDRESSES FOR THE LIVE QUERY").small().strong().color(DIM));
        hint(ui, "Derived from the .snap tree. Change these only if `probe` shows your firmware differs.");
        let live = &mut patch.live;
        text_row(ui, "Output source group", &mut live.out_source_group);
        text_row(ui, "Output source index", &mut live.out_source_index);
        text_row(ui, "Channel input group", &mut live.channel_input_group);
        text_row(ui, "Channel input index", &mut live.channel_input_index);
        text_row(ui, "Object name", &mut live.object_name);
        text_row(ui, "Input name", &mut live.input_name);
        num_row(ui, "Collect replies for (ms)", &mut live.settle_ms, 200..=10_000);

        ui.add_space(8.0);
        ui.label(egui::RichText::new("OUTPUTS PER GROUP").small().strong().color(DIM));
        egui::Grid::new("group_sizes").num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
            for (i, (group, size)) in live.group_sizes.iter_mut().enumerate() {
                ui.label(egui::RichText::new(group).monospace().color(TEXT));
                ui.add(egui::DragValue::new(size).range(1..=128));
                if i % 2 == 1 {
                    ui.end_row();
                }
            }
        });
    }

    /// A press on the console fills the row that asked for it.
    fn take_learned_press(&mut self, console: &[ConsoleActivity]) {
        let Some(index) = self.learning else { return };
        // Only a press counts; the release that follows it would overwrite.
        let Some(event) = console
            .iter()
            .filter(|e| e.seq > self.learn_from && e.value >= 0.5)
            .max_by_key(|e| e.seq)
        else {
            return;
        };
        if let Some(button) = self.draft.transport.buttons.get_mut(index) {
            button.address = event.address.clone();
            self.status = Some(format!("Learned {}", event.address));
        }
        self.learning = None;
    }

    fn transport(&mut self, ui: &mut egui::Ui, console: &[ConsoleActivity]) {
        heading(ui, "Transport", "Console buttons that drive the DAW, and lights that follow it.");
        check_row(ui, "Transport control", &mut self.draft.transport.enabled);

        ui.add_space(8.0);
        ui.label(egui::RichText::new("BUTTONS").small().strong().color(DIM));
        hint(ui, "Press learn, then the button on the console - or pick one out of what it is sending, below.");
        let mut remove = None;
        let mut learn = None;
        let learning = self.learning;
        for (i, button) in self.draft.transport.buttons.iter_mut().enumerate() {
            let waiting = learning == Some(i);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut button.address)
                        .desired_width(200.0)
                        .font(egui::TextStyle::Monospace),
                );
                let label = if waiting { "press it now" } else { "learn" };
                let text = egui::RichText::new(label).color(if waiting { ACCENT } else { DIM });
                if ui.add(egui::Button::new(text).small()).clicked() {
                    learn = Some(if waiting { None } else { Some(i) });
                }
                action_editor(ui, i, &mut button.action);
                ui.add(egui::DragValue::new(&mut button.threshold).speed(0.05).range(0.0..=1.0));
                if ui.small_button("remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(target) = learn {
            self.learning = target;
            self.learn_from = console.iter().map(|e| e.seq).max().unwrap_or(0);
            self.status = target.map(|_| "Press the button on the console.".to_string());
        }
        if let Some(i) = remove {
            self.draft.transport.buttons.remove(i);
        }
        if ui.button("Add a button").clicked() {
            self.draft.transport.buttons.push(ButtonMap {
                address: "/$ctl/user/1/bu/1".into(),
                threshold: 0.5,
                action: Action::TogglePlay,
            });
        }

        ui.add_space(10.0);
        ui.label(egui::RichText::new("WHAT THE CONSOLE IS SENDING").small().strong().color(DIM));
        if console.is_empty() {
            hint(ui, "Nothing yet. Touch a control on the console and it appears here.");
        } else {
            egui::ScrollArea::vertical()
                .id_salt("console_activity")
                .max_height(120.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let mut pick = None;
                    for event in console.iter().rev().take(20) {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&event.address)
                                    .monospace()
                                    .size(12.0)
                                    .color(TEXT),
                            );
                            ui.label(
                                egui::RichText::new(format!("{:.2}", event.value))
                                    .monospace()
                                    .small()
                                    .color(DIM),
                            );
                            if ui.small_button("use").clicked() {
                                pick = Some(event.address.clone());
                            }
                        });
                    }
                    if let Some(address) = pick {
                        // Fill the row being learned, or add one for it.
                        match self.learning.and_then(|i| self.draft.transport.buttons.get_mut(i)) {
                            Some(button) => button.address = address.clone(),
                            None => self.draft.transport.buttons.push(ButtonMap {
                                address: address.clone(),
                                threshold: 0.5,
                                action: Action::TogglePlay,
                            }),
                        }
                        self.learning = None;
                        self.status = Some(format!("Bound {address}"));
                    }
                });
        }

        ui.add_space(10.0);
        ui.label(egui::RichText::new("LIGHTS").small().strong().color(DIM));
        let mut remove = None;
        for (i, led) in self.draft.transport.leds.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt(("led", i))
                    .selected_text(led_label(led.source))
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for source in [
                            LedSource::Playing,
                            LedSource::Recording,
                            LedSource::Stopped,
                            LedSource::Looping,
                            LedSource::PunchIn,
                            LedSource::PunchOut,
                            LedSource::Click,
                        ] {
                            ui.selectable_value(&mut led.source, source, led_label(source));
                        }
                    });
                ui.add(
                    egui::TextEdit::singleline(&mut led.address)
                        .desired_width(230.0)
                        .font(egui::TextStyle::Monospace),
                );
                arg_editor(ui, ("on", i), &mut led.on);
                arg_editor(ui, ("off", i), &mut led.off);
                if ui.small_button("remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.draft.transport.leds.remove(i);
        }
        if ui.button("Add a light").clicked() {
            self.draft.transport.leds.push(LedMap {
                source: LedSource::Playing,
                address: "/$ctl/user/1/bu/1/led".into(),
                on: Arg::Int(1),
                off: Arg::Int(0),
            });
        }

        ui.add_space(10.0);
        ui.label(egui::RichText::new("RECORD ARM").small().strong().color(DIM));
        let mut remove = None;
        for (i, arm) in self.draft.transport.rec_arm.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut arm.address)
                        .desired_width(210.0)
                        .font(egui::TextStyle::Monospace),
                );
                ui.label(egui::RichText::new("channel").small().color(DIM));
                ui.add(egui::DragValue::new(&mut arm.channel).range(0..=96));
                ui.label(egui::RichText::new("strip").small().color(DIM));
                ui.add(egui::DragValue::new(&mut arm.strip).range(0..=512));
                ui.checkbox(&mut arm.follow_value, "latch");
                if ui.small_button("remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.draft.transport.rec_arm.remove(i);
        }
        if ui.button("Add a record arm").clicked() {
            self.draft.transport.rec_arm.push(RecArmMap {
                address: "/$ctl/user/2/bu/1".into(),
                threshold: 0.5,
                strip: 0,
                channel: 1,
                follow_value: true,
            });
        }
    }

    fn scenes(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Scenes", "Console scenes tied to session markers.");
        let scenes = &mut self.draft.scenes;
        check_row(ui, "Scene linking", &mut scenes.enabled);
        ui.horizontal(|ui| {
            theme::field(ui, "Direction");
            ui.selectable_value(&mut scenes.direction, SceneDirection::SceneToMarker, "scene -> marker");
            ui.selectable_value(&mut scenes.direction, SceneDirection::MarkerToScene, "marker -> scene");
            ui.selectable_value(&mut scenes.direction, SceneDirection::Bidirectional, "both");
        });
        text_row(ui, "Scene address", &mut scenes.scene_address);
        text_row(ui, "Recall address", &mut scenes.recall_address);
        check_row(ui, "Roll after locating", &mut scenes.locate_and_play);
        check_row(ui, "Drop a marker when rolling", &mut scenes.add_marker_while_rolling);
        check_row(ui, "Act on the same scene twice", &mut scenes.retrigger_same_scene);
        num_row(ui, "Ignore repeats within (ms)", &mut scenes.retrigger_guard_ms, 0..=30_000);

        ui.add_space(8.0);
        ui.label(egui::RichText::new("SCENE TO MARKER").small().strong().color(DIM));
        let mut remove = None;
        for (i, entry) in scenes.map.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut entry.scene).range(0..=999));
                ui.add(egui::TextEdit::singleline(&mut entry.marker).desired_width(240.0));
                if ui.small_button("remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            scenes.map.remove(i);
        }
        if ui.button("Add a scene").clicked() {
            let next = scenes.map.iter().map(|m| m.scene).max().unwrap_or(0) + 1;
            scenes.map.push(SceneMarker { scene: next, marker: String::new() });
        }
    }

    fn timecode(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Timecode", "The clock the bridge reads, stamps and locates by.");
        let tc = &mut self.draft.timecode;
        ui.horizontal(|ui| {
            theme::field(ui, "Frame rate");
            let label = tc.fps.map(|f| f.label().to_string()).unwrap_or_else(|| "from the session".into());
            egui::ComboBox::from_id_salt("fps").selected_text(label).width(190.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut tc.fps, None, "from the session");
                for fps in Fps::ALL {
                    ui.selectable_value(&mut tc.fps, Some(fps), fps.label());
                }
            });
        });
        let mut offset = tc.offset.clone().unwrap_or_default();
        ui.horizontal(|ui| {
            theme::field(ui, "Session start");
            let response = ui.add(
                egui::TextEdit::singleline(&mut offset)
                    .hint_text("from the session")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(140.0),
            );
            if response.changed() {
                tc.offset = if offset.trim().is_empty() { None } else { Some(offset.clone()) };
            }
            if !offset.trim().is_empty() && crate::timecode::Timecode::parse(&offset).is_none() {
                ui.label(egui::RichText::new("hours:minutes:seconds:frames").small().color(AMBER));
            }
        });
        text_row(ui, "Marker name", &mut tc.marker_template);
        hint(ui, "{tc} the timecode, {samples} the playhead, {n} a running count. Empty leaves markers unnamed.");
        check_row(ui, "Keep a show log", &mut tc.log);
        check_row(ui, "Log takes as well", &mut tc.log_transport);
        num_row_usize(ui, "Log lines kept", &mut tc.log_limit, 100..=100_000);

        if self.draft.livetrax.feedback & 64 == 0 {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "LiveTrax only sends timecode when the \"timecode\" feedback bit is on - \
                     switch it on under LiveTrax, or the clock here is worked out from the \
                     sample position instead.",
                )
                .small()
                .color(AMBER),
            );
        }
    }

    fn snapshot(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Snapshots", "Writing console names out of a session.");
        let snap = &mut self.draft.snapshot;
        text_row(ui, "Line format", &mut snap.line);
        hint(ui, "{path} the address, {name} the quoted name, {raw} unquoted, {ch} the channel.");
        check_row(ui, "Write a header comment", &mut snap.include_comments);
        num_row(ui, "First channel", &mut snap.first_channel, 1..=96);
        check_row(ui, "Include busses", &mut snap.include_busses);
        string_list(ui, "Name paths in a template", &mut snap.name_paths, "/ch/{ch}/name");
    }

    fn mapping(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Manual map", "Used when no output patch is loaded.");
        let map = &mut self.draft.map;
        check_row(ui, "Channel N to strip N", &mut map.one_to_one);
        num_row_i32(ui, "Strip offset", &mut map.strip_offset, -128..=128);
        ui.add_space(6.0);
        ui.label(egui::RichText::new("OVERRIDES").small().strong().color(DIM));
        let mut remove = None;
        for (i, pair) in map.pairs.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("channel").small().color(DIM));
                ui.add(egui::DragValue::new(&mut pair.channel).range(1..=96));
                ui.label(egui::RichText::new("strip").small().color(DIM));
                ui.add(egui::DragValue::new(&mut pair.strip).range(1..=512));
                if ui.small_button("remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            map.pairs.remove(i);
        }
        if ui.button("Add a pair").clicked() {
            map.pairs.push(Pair { channel: 1, strip: 1 });
        }
    }
}

// ------------------------------------------------------------- field bits --

fn heading(ui: &mut egui::Ui, title: &str, blurb: &str) {
    ui.heading(title);
    ui.label(egui::RichText::new(blurb).small().color(DIM));
    ui.add_space(8.0);
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        theme::field(ui, "");
        ui.label(egui::RichText::new(text).small().color(DIM));
    });
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        theme::field(ui, label);
        ui.add(egui::TextEdit::singleline(value).desired_width(300.0));
    });
}

fn check_row(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    ui.horizontal(|ui| {
        theme::field(ui, "");
        ui.checkbox(value, label);
    });
}

fn num_row<T>(ui: &mut egui::Ui, label: &str, value: &mut T, range: std::ops::RangeInclusive<T>)
where
    T: egui::emath::Numeric,
{
    ui.horizontal(|ui| {
        theme::field(ui, label);
        ui.add(egui::DragValue::new(value).range(range));
    });
}

fn num_row_usize(ui: &mut egui::Ui, label: &str, value: &mut usize, range: std::ops::RangeInclusive<usize>) {
    num_row(ui, label, value, range);
}

fn num_row_i32(ui: &mut egui::Ui, label: &str, value: &mut i32, range: std::ops::RangeInclusive<i32>) {
    num_row(ui, label, value, range);
}

fn num_row_f64(ui: &mut egui::Ui, label: &str, value: &mut f64, range: std::ops::RangeInclusive<f64>) {
    num_row(ui, label, value, range);
}

fn path_row(ui: &mut egui::Ui, label: &str, value: &mut Option<std::path::PathBuf>, filter: &[&str]) {
    ui.horizontal(|ui| {
        theme::field(ui, label);
        let mut text = value.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        if ui.add(egui::TextEdit::singleline(&mut text).desired_width(300.0)).changed() {
            *value = if text.trim().is_empty() { None } else { Some(text.trim().into()) };
        }
        if ui.small_button("Browse").clicked() {
            let mut dialog = rfd::FileDialog::new();
            if !filter.is_empty() {
                dialog = dialog.add_filter("supported", filter);
            }
            if let Some(picked) = dialog.pick_file() {
                *value = Some(picked);
            }
        }
        if value.is_some() && ui.small_button("clear").clicked() {
            *value = None;
        }
    });
}

/// A list of free-text values with add and remove.
fn string_list(ui: &mut egui::Ui, label: &str, values: &mut Vec<String>, example: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(label.to_uppercase()).small().strong().color(DIM));
    let mut remove = None;
    for (i, value) in values.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(value)
                    .desired_width(260.0)
                    .font(egui::TextStyle::Monospace),
            );
            if ui.small_button("remove").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        values.remove(i);
    }
    if ui.button("Add").clicked() {
        values.push(example.to_string());
    }
}

/// Bit flags as named checkboxes, three to a row.
fn bitmask(ui: &mut egui::Ui, label: &str, value: &mut u32, bits: &[(u32, &str)]) {
    ui.label(egui::RichText::new(label.to_uppercase()).small().strong().color(DIM));
    egui::Grid::new(label).num_columns(3).spacing([14.0, 3.0]).show(ui, |ui| {
        for (i, (bit, name)) in bits.iter().enumerate() {
            let mut on = *value & bit != 0;
            if ui.checkbox(&mut on, *name).changed() {
                if on {
                    *value |= bit;
                } else {
                    *value &= !bit;
                }
            }
            if i % 3 == 2 {
                ui.end_row();
            }
        }
    });
}

fn arg_editor(ui: &mut egui::Ui, id: (&str, usize), arg: &mut Arg) {
    let mut text = match arg {
        Arg::Bool(b) => b.to_string(),
        Arg::Int(i) => i.to_string(),
        Arg::Float(f) => f.to_string(),
        Arg::Str(s) => s.clone(),
    };
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id_salt(id)
            .desired_width(56.0)
            .font(egui::TextStyle::Monospace),
    );
    if response.changed() {
        // Keep the type the text implies, the way the config file would.
        *arg = if let Ok(i) = text.parse::<i32>() {
            Arg::Int(i)
        } else if let Ok(f) = text.parse::<f32>() {
            Arg::Float(f)
        } else if let Ok(b) = text.parse::<bool>() {
            Arg::Bool(b)
        } else {
            Arg::Str(text)
        };
    }
}

/// A label and the action it stands for.
type SimpleAction = (&'static str, fn() -> Action);

/// The simple actions in a combo; the two that carry text get a field.
fn action_editor(ui: &mut egui::Ui, index: usize, action: &mut Action) {
    const SIMPLE: [SimpleAction; 18] = [
        ("play", || Action::Play),
        ("stop", || Action::Stop),
        ("toggle roll", || Action::TogglePlay),
        ("record arm", || Action::RecordArmToggle),
        ("record + roll", || Action::RecordStart),
        ("arm every track", || Action::AllRecEnable),
        ("go to start", || Action::GotoStart),
        ("go to end", || Action::GotoEnd),
        ("wind back", || Action::Rewind),
        ("wind forward", || Action::FastForward),
        ("next marker", || Action::NextMarker),
        ("previous marker", || Action::PrevMarker),
        ("drop marker", || Action::AddMarker),
        ("loop", || Action::LoopToggle),
        ("punch in", || Action::PunchIn),
        ("punch out", || Action::PunchOut),
        ("click", || Action::ClickToggle),
        ("MIDI panic", || Action::MidiPanic),
    ];
    let current = action_label(action);
    egui::ComboBox::from_id_salt(("action", index))
        .selected_text(current)
        .width(150.0)
        .show_ui(ui, |ui| {
            for (label, make) in SIMPLE {
                if ui.selectable_label(action_label(action) == label, label).clicked() {
                    *action = make();
                }
            }
            if ui.selectable_label(matches!(action, Action::LocateMarker(_)), "locate marker").clicked()
            {
                *action = Action::LocateMarker(String::new());
            }
            if ui.selectable_label(matches!(action, Action::JumpBars(_)), "jump bars").clicked() {
                *action = Action::JumpBars(1.0);
            }
            if ui.selectable_label(matches!(action, Action::JumpSeconds(_)), "jump seconds").clicked()
            {
                *action = Action::JumpSeconds(10.0);
            }
            if ui.selectable_label(matches!(action, Action::SetSpeed(_)), "play at speed").clicked() {
                *action = Action::SetSpeed(1.0);
            }
            if ui.selectable_label(matches!(action, Action::AccessAction(_)), "DAW action").clicked() {
                *action = Action::AccessAction(String::new());
            }
        });
    match action {
        Action::LocateMarker(name) | Action::AccessAction(name) => {
            ui.add(egui::TextEdit::singleline(name).desired_width(140.0));
        }
        Action::JumpBars(amount) | Action::JumpSeconds(amount) => {
            ui.add(egui::DragValue::new(amount).speed(0.5).range(-240.0..=240.0));
        }
        Action::SetSpeed(speed) => {
            ui.add(egui::DragValue::new(speed).speed(0.1).range(-8.0..=8.0).suffix("x"));
        }
        Action::Osc { address, .. } => {
            ui.label(egui::RichText::new(format!("{address} (edit in the file)")).small().color(DIM));
        }
        _ => {}
    }
}

fn led_label(source: LedSource) -> &'static str {
    match source {
        LedSource::Playing => "rolling",
        LedSource::Recording => "recording",
        LedSource::Stopped => "stopped",
        LedSource::Looping => "looping",
        LedSource::PunchIn => "punch in",
        LedSource::PunchOut => "punch out",
        LedSource::Click => "click",
    }
}

fn action_label(action: &Action) -> &'static str {
    match action {
        Action::Play => "play",
        Action::Stop => "stop",
        Action::TogglePlay => "toggle roll",
        Action::RecordArmToggle => "record arm",
        Action::RecordStart => "record + roll",
        Action::GotoStart => "go to start",
        Action::GotoEnd => "go to end",
        Action::NextMarker => "next marker",
        Action::PrevMarker => "previous marker",
        Action::AddMarker => "drop marker",
        Action::LocateMarker(_) => "locate marker",
        Action::AccessAction(_) => "DAW action",
        Action::FastForward => "wind forward",
        Action::Rewind => "wind back",
        Action::LoopToggle => "loop",
        Action::PunchIn => "punch in",
        Action::PunchOut => "punch out",
        Action::ClickToggle => "click",
        Action::AllRecEnable => "arm every track",
        Action::MidiPanic => "MIDI panic",
        Action::JumpBars(_) => "jump bars",
        Action::JumpSeconds(_) => "jump seconds",
        Action::SetSpeed(_) => "play at speed",
        Action::Osc { .. } => "raw OSC",
    }
}
