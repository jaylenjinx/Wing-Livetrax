//! The sync engine: console events in one side, DAW events in the other, with
//! echo suppression so a value never ping-pongs between the two.

use anyhow::{Context, Result};
use rosc::OscType;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::config::{Action, ChannelMap, Config, LedSource, PatchSource, RecArmMap};
use crate::livetrax::Daw;
use crate::markers::{self, MarkerTable};
use crate::osc::{self, Incoming};
use crate::session;
use crate::shared::{Command, CommandRx, ConsoleActivity, Cue, CueKind, Shared};
use crate::timecode::{self, Fps};
use crate::patch::PatchModel;
use crate::snapfile::SnapFile;
use crate::console::{Console, ConsoleEvent};

/// How long a value we pushed is ignored when it comes straight back.
const ECHO_WINDOW: Duration = Duration::from_millis(1_500);
/// How long after a scene-driven locate a marker report is ignored.
const SCENE_LOOP_WINDOW: Duration = Duration::from_millis(1_500);

pub struct Bridge {
    cfg: Config,
    console: Console,
    daw: Daw,
    map: ChannelMap,
    markers: MarkerTable,
    st: State,
    /// Present when a GUI is attached.
    shared: Option<Shared>,
    /// Output patch the DAW records through, when one is configured.
    patch: Option<PatchInfo>,
    /// Patch being assembled from console replies.
    live: PatchModel,
    live_query: Option<LiveQuery>,
}

/// A patch query in flight. Outputs and channel wiring come back first; the
/// second pass asks about the few sockets no channel claimed.
struct LiveQuery {
    group: String,
    phase: u8,
    deadline: Instant,
    asked: usize,
}

/// The console output patch that decides which channel owns which DAW strip.
struct PatchInfo {
    /// Where it came from: a file name, or "console".
    source: String,
    group: String,
    outputs: usize,
    mapped: usize,
    /// The resolved patch, for naming new sessions and for display.
    slots: Vec<crate::patch::Slot>,
}

impl PatchInfo {
    fn summary(&self) -> String {
        format!(
            "{} {} - {} of {} outputs carry a channel ({})",
            self.source,
            self.group,
            self.mapped,
            self.outputs,
            crate::snapfile::group_label(&self.group)
        )
    }
}

#[derive(Default)]
struct State {
    /// ssid -> name as the DAW last reported it.
    strips: BTreeMap<u32, String>,
    /// wing channel -> name as the console last reported it.
    wing_names: BTreeMap<u16, String>,
    pending_daw: HashMap<u32, (String, Instant)>,
    pending_wing: HashMap<u16, (String, Instant)>,
    sent_daw: HashMap<u32, (String, Instant)>,
    sent_wing: HashMap<u16, (String, Instant)>,
    /// Renames awaiting confirmation, for capability detection.
    rename_watch: Vec<(u32, String, Instant)>,
    rename_warned: bool,
    playing: bool,
    recording: bool,
    looping: bool,
    punch_in: bool,
    punch_out: bool,
    click: bool,
    speed: f32,
    session_end: i64,
    console_events: Vec<ConsoleActivity>,
    console_seq: u64,
    position: i64,
    current_scene: Option<i32>,
    /// Scene we last acted on, for repeat rate-limiting.
    last_scene_act: Option<(i32, Instant)>,
    /// Scene we pushed to the console, so its confirmation is not acted on.
    scene_echo: Option<(i32, Instant)>,
    current_marker: Option<String>,
    last_scene_locate: Option<Instant>,
    strip_list_seen: bool,
    /// Timecode as the DAW reports it, and the session's own frame rate.
    timecode: Option<String>,
    session_fps: Option<crate::timecode::Fps>,
    session_offset_samples: i64,
    markers_dropped: usize,
    cues: Vec<crate::shared::Cue>,
    last_wing_rx: Option<Instant>,
    last_daw_rx: Option<Instant>,
    wing_msgs: u64,
    daw_msgs: u64,
}

impl Bridge {
    pub fn new(cfg: Config, console: Console, daw: Daw) -> Self {
        let map = ChannelMap::build(&cfg.map, cfg.channels());
        let mut markers = MarkerTable::default();
        markers.sample_rate = Some(cfg.livetrax.sample_rate);
        let mut bridge = Self {
            cfg,
            console,
            daw,
            map,
            markers,
            st: State::default(),
            shared: None,
            patch: None,
            live: PatchModel::default(),
            live_query: None,
        };
        bridge.load_patch();
        bridge
    }

    /// Read the configured output patch and rebuild the channel map from it.
    fn load_patch(&mut self) {
        if self.cfg.patch.source == PatchSource::Console {
            return; // the console is asked instead; see start_live_query
        }
        self.patch = None;
        let Some(path) = self.cfg.patch.snap_file.clone() else {
            return;
        };
        match SnapFile::load(&path) {
            Ok(snap) => {
                let group = self.cfg.patch.output_group.clone();
                let slots = snap.outputs(&group);
                if slots.is_empty() {
                    tracing::warn!(
                        "output group {group:?} is not in {} - patch ignored",
                        path.display()
                    );
                    return;
                }
                let mapped = slots.iter().filter(|s| s.channel.is_some()).count();
                let info = PatchInfo {
                    outputs: slots.len(),
                    mapped,
                    slots: slots.clone(),
                    source: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                    group,
                };
                tracing::info!("patch: {}", info.summary());
                if self.cfg.patch.use_for_map {
                    self.map = ChannelMap::from_outputs(&slots, self.cfg.map.strip_offset);
                }
                self.patch = Some(info);
            }
            Err(e) => tracing::warn!("output patch: {e:#}"),
        }
    }

    /// Publish state to, and take commands from, a GUI.
    pub fn attach_ui(mut self, shared: Shared) -> Self {
        self.shared = Some(shared);
        self
    }

    pub async fn run(
        mut self,
        mut console_rx: mpsc::Receiver<ConsoleEvent>,
        mut daw_rx: mpsc::Receiver<Incoming>,
        mut cmd_rx: CommandRx,
    ) -> Result<()> {
        tracing::info!(
            "channel map: {} pairs, names {:?}/{}, scenes {}",
            self.map.len(),
            self.cfg.names.direction,
            if self.cfg.names.enabled { "on" } else { "off" },
            if self.cfg.scenes.enabled { "on" } else { "off" }
        );

        self.load_session_markers();
        self.publish();
        let (file_tx, mut file_rx) = mpsc::channel::<()>(8);
        let _watcher = self.spawn_session_watcher(file_tx);

        // Bring both ends up immediately, then keep them alive on timers.
        self.console.start().await.ok();
        if self.cfg.patch.source == PatchSource::Console && self.cfg.patch.query_on_start {
            // Names first: the patch resolver uses them.
            self.console.query_all_names(self.cfg.channels()).await.ok();
            if let Err(e) = self.start_live_query(None).await {
                tracing::warn!("patch query: {e:#}");
            }
        }
        self.daw.set_surface().await.ok();
        self.daw.request_strip_list().await.ok();
        if self.cfg.names.enabled {
            self.console.query_all_names(self.cfg.channels()).await.ok();
        }

        // A WING wants its subscription renewed; a Qu hangs up without an
        // Active Sense. Same timer, different reason.
        let mut keepalive = tokio::time::interval(self.console.keepalive_interval(&self.cfg));
        let mut refresh = tokio::time::interval(Duration::from_millis(
            self.cfg.livetrax.refresh_interval_ms.max(1_000),
        ));
        let poll_ms = self.cfg.name_poll_interval_ms();
        let names_on = self.cfg.names.enabled;
        let mut name_poll = tokio::time::interval(Duration::from_millis(poll_ms.max(1_000)));
        let mut flush = tokio::time::interval(Duration::from_millis(50));

        loop {
            tokio::select! {
                Some(event) = console_rx.recv() => {
                    if let Err(e) = self.on_console(event).await {
                        tracing::warn!("console event: {e:#}");
                    }
                }
                Some(inc) = daw_rx.recv() => {
                    if let Err(e) = self.on_daw(inc).await {
                        tracing::warn!("daw event: {e:#}");
                    }
                }
                Some(()) = file_rx.recv() => {
                    self.load_session_markers();
                }
                Some(cmd) = cmd_rx.recv() => {
                    if let Err(e) = self.on_command(cmd).await {
                        tracing::warn!("command: {e:#}");
                    }
                }
                _ = keepalive.tick() => { self.console.keepalive().await.ok(); }
                _ = refresh.tick() => {
                    self.daw.set_surface().await.ok();
                    self.daw.request_strip_list().await.ok();
                }
                _ = name_poll.tick(), if poll_ms > 0 && names_on => {
                    self.console.query_all_names(self.cfg.channels()).await.ok();
                }
                _ = flush.tick() => {
                    if let Err(e) = self.flush_names().await {
                        tracing::warn!("name flush: {e:#}");
                    }
                    if let Err(e) = self.tick_live_query().await {
                        tracing::warn!("patch query: {e:#}");
                    }
                    self.check_rename_capability();
                    self.publish();
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    return Ok(());
                }
            }
        }
    }

    // ------------------------------------------------------------- console --

    async fn on_console(&mut self, event: ConsoleEvent) -> Result<()> {
        self.st.last_wing_rx = Some(Instant::now());
        self.st.wing_msgs += 1;

        match event {
            ConsoleEvent::Name { channel, name } => {
                // Names are always tracked - the interface and the session
                // generator need them even when sync is off or pointed the
                // other way.
                self.on_wing_name(channel, name.trim());
            }
            ConsoleEvent::Scene { index } => {
                if self.cfg.scenes.enabled && self.cfg.scenes.direction.scene_to_marker() {
                    self.on_scene(index).await?;
                }
            }
            ConsoleEvent::Control { id, value } => {
                self.remember_console_event(&id, value);
                self.on_control(&id, value).await?;
            }
            ConsoleEvent::Osc(msg) => {
                let addr = msg.addr.as_str();
                tracing::trace!("console -> {}", osc::render(&msg));
                let value = msg.args.first().and_then(osc::as_f32).unwrap_or(1.0);

                // Patch replies only matter while a query is outstanding.
                if self.live_query.is_some()
                    && crate::patch::absorb(&mut self.live, &self.cfg.patch.live, addr, &msg.args)
                {
                    return Ok(());
                }

                // Whatever is left is a control someone touched, so it is
                // worth remembering: this is the list a binding is learned
                // from.
                self.remember_console_event(addr, value);
                self.on_control(addr, value).await?;

                // A WING reports its scene as a value at a configured address,
                // rather than as a scene message of its own.
                if self.cfg.scenes.enabled
                    && self.cfg.scenes.direction.scene_to_marker()
                    && addr == self.cfg.scenes.scene_address
                {
                    if let Some(index) = msg.args.first().and_then(osc::as_i64) {
                        self.on_scene(index as i32).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// A control was touched: run whatever is bound to it.
    async fn on_control(&mut self, id: &str, value: f32) -> Result<()> {
        if !self.cfg.transport.enabled {
            return Ok(());
        }
        // Cloned so the borrow of self.cfg ends before the action runs.
        let hits: Vec<Action> = self
            .cfg
            .transport
            .buttons
            .iter()
            .filter(|b| b.address == id && (b.threshold <= 0.0 || value >= b.threshold))
            .map(|b| b.action.clone())
            .collect();
        for action in hits {
            tracing::info!("console {id} -> {action:?}");
            self.run_action(&action).await?;
        }

        let arms: Vec<RecArmMap> = self
            .cfg
            .transport
            .rec_arm
            .iter()
            .filter(|r| r.address == id)
            .cloned()
            .collect();
        for arm in arms {
            let ssid = if arm.strip > 0 {
                arm.strip
            } else {
                self.map.strip(arm.channel).unwrap_or(0)
            };
            if ssid == 0 {
                tracing::warn!("rec_arm {id}: no strip for channel {}", arm.channel);
                continue;
            }
            let on = if arm.follow_value { value >= arm.threshold } else { true };
            tracing::info!("console {id} -> strip {ssid} rec {}", on as i32);
            self.daw.rec_enable_strip(ssid, on).await?;
        }
        Ok(())
    }

    /// Keep the last few console controls that were touched. Channel names and
    /// patch replies are excluded - this is the list you pick a button from.
    fn remember_console_event(&mut self, addr: &str, value: f32) {
        self.st.console_seq += 1;
        let seq = self.st.console_seq;
        if let Some(existing) = self.st.console_events.iter_mut().find(|e| e.address == addr) {
            existing.seq = seq;
            existing.value = value;
        } else {
            self.st.console_events.push(ConsoleActivity {
                seq,
                address: addr.to_string(),
                value,
            });
        }
        self.st.console_events.sort_by_key(|e| e.seq);
        while self.st.console_events.len() > 60 {
            self.st.console_events.remove(0);
        }
    }

    fn on_wing_name(&mut self, ch: u16, name: &str) {
        let previous = self.st.wing_names.insert(ch, name.to_string());
        if previous.as_deref() == Some(name) {
            return;
        }
        if !(self.cfg.names.enabled && self.cfg.names.direction.wing_to_daw()) {
            return;
        }
        // Ignore the console echoing back what we just wrote to it.
        if let Some((sent, at)) = self.st.sent_wing.get(&ch) {
            if sent == name && at.elapsed() < ECHO_WINDOW {
                return;
            }
        }
        let Some(ssid) = self.map.strip(ch) else { return };
        if !self.propagate_worthy(name) {
            return;
        }
        let target = format!(
            "{}{}{}",
            self.cfg.names.daw_prefix, name, self.cfg.names.daw_suffix
        );
        if self.st.strips.get(&ssid).map(String::as_str) == Some(target.as_str()) {
            return;
        }
        tracing::debug!("queue ch{ch} \"{name}\" -> strip {ssid}");
        self.st.pending_daw.insert(ssid, (target, Instant::now()));
    }

    async fn on_scene(&mut self, idx: i32) -> Result<()> {
        // The console confirming a recall the bridge itself sent.
        if let Some((scene, at)) = self.st.scene_echo {
            if scene == idx && at.elapsed() < ECHO_WINDOW {
                return Ok(());
            }
        }
        let same = self.st.current_scene == Some(idx);
        self.st.current_scene = Some(idx);
        if same && !self.cfg.scenes.retrigger_same_scene {
            // Subscription refreshes re-report the active scene; ignore those.
            return Ok(());
        }
        let guard = Duration::from_millis(self.cfg.scenes.retrigger_guard_ms);
        if self
            .st
            .last_scene_act
            .map(|(scene, at)| scene == idx && at.elapsed() < guard)
            .unwrap_or(false)
        {
            return Ok(());
        }
        self.st.last_scene_act = Some((idx, Instant::now()));
        let Some(entry) = self.cfg.scenes.map.iter().find(|m| m.scene == idx).cloned() else {
            tracing::info!("scene {idx} recalled, no marker mapped");
            return Ok(());
        };

        if self.st.playing && self.cfg.scenes.add_marker_while_rolling {
            tracing::info!("scene {idx} while rolling -> marker \"{}\"", entry.marker);
            self.daw.add_marker(Some(&entry.marker)).await?;
            self.markers.observe(&entry.marker, self.st.position);
            self.log_cue(CueKind::Scene, format!("scene {idx}: {}", entry.marker));
            return Ok(());
        }

        match self.markers.find(&entry.marker) {
            Some(marker) => {
                let start = marker.start;
                tracing::info!(
                    "scene {idx} -> locate \"{}\" @ {} samples",
                    entry.marker,
                    start
                );
                self.st.last_scene_locate = Some(Instant::now());
                self.daw.locate(start, self.cfg.scenes.locate_and_play).await?;
                self.log_cue(CueKind::Scene, format!("scene {idx}: {}", entry.marker));
            }
            None => {
                tracing::warn!(
                    "scene {idx}: marker \"{}\" not found. Known markers: {:?}",
                    entry.marker,
                    self.markers.names()
                );
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------------- daw --

    async fn on_daw(&mut self, inc: Incoming) -> Result<()> {
        self.st.last_daw_rx = Some(Instant::now());
        self.st.daw_msgs += 1;
        let addr = inc.msg.addr.as_str();
        let args = &inc.msg.args;
        tracing::trace!("daw -> {}", osc::render(&inc.msg));

        match addr {
            "#reply" | "/reply" => self.on_strip_reply(args),
            "/heartbeat" => {}
            "/transport_play" => {
                let on = args.first().and_then(osc::as_f32).unwrap_or(1.0) > 0.5;
                self.set_playing(on).await?;
            }
            "/transport_stop" => {
                let on = args.first().and_then(osc::as_f32).unwrap_or(1.0) > 0.5;
                if on {
                    self.set_playing(false).await?;
                }
            }
            "/rec_enable_toggle" | "/record_tally" | "/record_enabled" | "/transport_record" => {
                let on = args.first().and_then(osc::as_f32).unwrap_or(0.0) > 0.5;
                self.set_recording(on).await?;
            }
            // The DAW echoes these back as its state changes, however it was
            // changed - from here, from its own window, or from a key.
            "/loop_toggle" | "/toggle_punch_in" | "/toggle_punch_out" | "/toggle_click" => {
                let on = args.first().and_then(osc::as_f32).unwrap_or(0.0) > 0.5;
                match addr {
                    "/loop_toggle" => self.st.looping = on,
                    "/toggle_punch_in" => self.st.punch_in = on,
                    "/toggle_punch_out" => self.st.punch_out = on,
                    _ => self.st.click = on,
                }
                self.update_leds().await?;
            }
            "/transport_speed" => {
                if let Some(speed) = args.first().and_then(osc::as_f32) {
                    self.st.speed = speed;
                }
            }
            "/position/samples" => {
                if let Some(p) = args.first().and_then(osc::as_i64) {
                    self.st.position = p;
                }
            }
            // Sent when the surface asks for timecode feedback (bit 64).
            "/position/smpte" | "/position/timecode" => {
                if let Some(tc) = args.first().and_then(osc::as_str) {
                    let tc = tc.trim();
                    if !tc.is_empty() {
                        self.st.timecode = Some(tc.to_string());
                    }
                }
            }
            "/marker" => {
                if let Some(name) = args.first().and_then(osc::as_str) {
                    self.on_marker(name.to_string()).await?;
                }
            }
            _ => {
                if let Some((ssid, name)) = self.parse_strip_name(addr, args) {
                    self.on_daw_name(ssid, &name);
                }
            }
        }
        Ok(())
    }

    /// `/strip/name <ssid> <name>` or, with the "ssid in path" feedback bit,
    /// `/strip/name/<ssid> <name>`.
    fn parse_strip_name(&self, addr: &str, args: &[OscType]) -> Option<(u32, String)> {
        if addr == "/strip/name" {
            let ssid = args.first().and_then(osc::as_i64)? as u32;
            let name = args.get(1).and_then(osc::as_str)?.to_string();
            return Some((ssid, name));
        }
        let rest = addr.strip_prefix("/strip/name/")?;
        let ssid = rest.parse::<u32>().ok()?;
        let name = args.first().and_then(osc::as_str)?.to_string();
        Some((ssid, name))
    }

    fn on_strip_reply(&mut self, args: &[OscType]) {
        let Some(kind) = args.first().and_then(osc::as_str) else { return };
        if kind == "end_route_list" {
            if let Some(sr) = args.get(1).and_then(osc::as_i64) {
                if sr > 0 {
                    self.markers.sample_rate = Some(sr as f64);
                }
            }
            // The reply ends with the session's last sample, which is what the
            // scrub bar needs for its range.
            if let Some(end) = args.get(2).and_then(osc::as_i64) {
                if end > 0 {
                    self.st.session_end = end;
                }
            }
            if !self.st.strip_list_seen {
                self.st.strip_list_seen = true;
                tracing::info!("strip list: {} strips known", self.st.strips.len());
            }
            return;
        }
        let (Some(ssid), Some(name)) = (
            args.get(1).and_then(osc::as_i64),
            args.get(2).and_then(osc::as_str),
        ) else {
            return;
        };
        self.on_daw_name(ssid as u32, name);
    }

    fn on_daw_name(&mut self, ssid: u32, name: &str) {
        let previous = self.st.strips.insert(ssid, name.to_string());
        // A rename we asked for has landed.
        self.st.rename_watch.retain(|(id, want, _)| !(*id == ssid && want == name));

        if previous.as_deref() == Some(name) {
            return;
        }
        if !(self.cfg.names.enabled && self.cfg.names.direction.daw_to_wing()) {
            return;
        }
        if let Some((sent, at)) = self.st.sent_daw.get(&ssid) {
            if sent == name && at.elapsed() < ECHO_WINDOW {
                return;
            }
        }
        let Some(ch) = self.map.channel(ssid) else { return };
        if !self.propagate_worthy(name) {
            return;
        }
        let target = self.truncate_for_wing(name);
        if self.st.wing_names.get(&ch).map(String::as_str) == Some(target.as_str()) {
            return;
        }
        tracing::debug!("queue strip {ssid} \"{name}\" -> ch{ch}");
        self.st.pending_wing.insert(ch, (target, Instant::now()));
    }

    async fn on_marker(&mut self, name: String) -> Result<()> {
        if self.st.current_marker.as_deref() == Some(name.as_str()) {
            return Ok(());
        }
        self.markers.observe(&name, self.st.position);
        self.st.current_marker = Some(name.clone());
        self.log_cue(CueKind::Marker, name.clone());

        if !(self.cfg.scenes.enabled && self.cfg.scenes.direction.marker_to_scene()) {
            return Ok(());
        }
        // Do not bounce a scene recall back at the console.
        if self
            .st
            .last_scene_locate
            .map(|t| t.elapsed() < SCENE_LOOP_WINDOW)
            .unwrap_or(false)
        {
            return Ok(());
        }
        let Some(entry) = self
            .cfg
            .scenes
            .map
            .iter()
            .find(|m| m.marker.eq_ignore_ascii_case(&name))
            .cloned()
        else {
            return Ok(());
        };
        if self.st.current_scene == Some(entry.scene) {
            return Ok(());
        }
        tracing::info!("marker \"{name}\" -> scene {}", entry.scene);
        self.st.current_scene = Some(entry.scene);
        self.st.scene_echo = Some((entry.scene, Instant::now()));
        self.console
            .recall_scene(&self.cfg.scenes.recall_address, entry.scene)
            .await?;
        Ok(())
    }

    async fn set_playing(&mut self, on: bool) -> Result<()> {
        if self.st.playing == on {
            return Ok(());
        }
        self.st.playing = on;
        tracing::info!("transport: {}", if on { "rolling" } else { "stopped" });
        if self.cfg.timecode.log_transport {
            let kind = if on { CueKind::TakeStart } else { CueKind::TakeStop };
            let detail = if self.st.recording { "recording" } else { "playback" };
            self.log_cue(kind, detail);
        }
        self.update_leds().await
    }

    async fn set_recording(&mut self, on: bool) -> Result<()> {
        if self.st.recording == on {
            return Ok(());
        }
        self.st.recording = on;
        tracing::info!("record arm: {}", if on { "on" } else { "off" });
        self.update_leds().await
    }

    async fn update_leds(&self) -> Result<()> {
        for led in &self.cfg.transport.leds {
            let on = match led.source {
                LedSource::Playing => self.st.playing,
                LedSource::Recording => self.st.recording,
                LedSource::Stopped => !self.st.playing,
                LedSource::Looping => self.st.looping,
                LedSource::PunchIn => self.st.punch_in,
                LedSource::PunchOut => self.st.punch_out,
                LedSource::Click => self.st.click,
            };
            let arg = if on { &led.on } else { &led.off };
            self.console.send_control(&led.address, arg).await?;
        }
        Ok(())
    }

    // -------------------------------------------------------------- actions --

    async fn run_action(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::Play => self.daw.play().await?,
            Action::Stop => self.daw.stop().await?,
            Action::TogglePlay => self.daw.toggle_roll().await?,
            Action::RecordArmToggle => self.daw.rec_enable_toggle().await?,
            Action::RecordStart => {
                if !self.st.recording {
                    self.daw.rec_enable_toggle().await?;
                }
                self.daw.play().await?;
            }
            Action::GotoStart => self.daw.goto_start().await?,
            Action::GotoEnd => self.daw.goto_end().await?,
            Action::NextMarker => self.daw.next_marker().await?,
            Action::PrevMarker => self.daw.prev_marker().await?,
            Action::AddMarker => {
                let name = self.marker_name();
                self.daw.add_marker(name.as_deref()).await?;
                if let Some(name) = name {
                    // The DAW echoes the marker back when it accepts the name;
                    // log it either way so the take sheet is complete.
                    self.log_cue(CueKind::Marker, name);
                }
            }
            Action::FastForward => self.daw.fast_forward().await?,
            Action::Rewind => self.daw.rewind().await?,
            Action::LoopToggle => self.daw.loop_toggle().await?,
            Action::PunchIn => self.daw.punch_in().await?,
            Action::PunchOut => self.daw.punch_out().await?,
            Action::ClickToggle => self.daw.click_toggle().await?,
            Action::AllRecEnable => self.daw.all_rec_enable().await?,
            Action::MidiPanic => self.daw.midi_panic().await?,
            Action::JumpBars(bars) => self.daw.jump_bars(*bars).await?,
            Action::JumpSeconds(seconds) => self.daw.jump_seconds(*seconds).await?,
            Action::SetSpeed(speed) => self.daw.set_speed(*speed).await?,
            Action::AccessAction(a) => self.daw.access_action(a).await?,
            Action::LocateMarker(name) => match self.markers.find(name) {
                Some(m) => {
                    let start = m.start;
                    self.daw.locate(start, self.cfg.scenes.locate_and_play).await?;
                }
                None => tracing::warn!(
                    "marker \"{name}\" unknown. Known markers: {:?}",
                    self.markers.names()
                ),
            },
            Action::Osc { address, args } => {
                let args = args.iter().map(|a| a.to_osc()).collect();
                self.daw.link.send(address.clone(), args).await?;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------- name flush --

    async fn flush_names(&mut self) -> Result<()> {
        let debounce = Duration::from_millis(self.cfg.names.debounce_ms);

        let ready: Vec<(u32, String)> = self
            .st
            .pending_daw
            .iter()
            .filter(|(_, (_, at))| at.elapsed() >= debounce)
            .map(|(ssid, (name, _))| (*ssid, name.clone()))
            .collect();
        for (ssid, name) in ready {
            self.st.pending_daw.remove(&ssid);
            tracing::info!("name -> strip {ssid}: \"{name}\"");
            self.daw.rename_strip(ssid, &name).await?;
            self.st.sent_daw.insert(ssid, (name.clone(), Instant::now()));
            self.st.rename_watch.push((ssid, name, Instant::now()));
        }

        let ready: Vec<(u16, String)> = self
            .st
            .pending_wing
            .iter()
            .filter(|(_, (_, at))| at.elapsed() >= debounce)
            .map(|(ch, (name, _))| (*ch, name.clone()))
            .collect();
        for (ch, name) in ready {
            self.st.pending_wing.remove(&ch);
            tracing::info!("name -> ch{ch}: \"{name}\"");
            self.console.set_name(ch, &name).await?;
            self.st.sent_wing.insert(ch, (name, Instant::now()));
        }
        Ok(())
    }

    /// LiveTrax builds differ on whether a strip can be renamed over OSC. If a
    /// rename is never confirmed by feedback, say so once instead of silently
    /// doing nothing forever.
    fn check_rename_capability(&mut self) {
        if self.st.rename_warned || self.st.rename_watch.is_empty() {
            return;
        }
        let stale = self
            .st
            .rename_watch
            .iter()
            .filter(|(_, _, at)| at.elapsed() > Duration::from_secs(6))
            .count();
        if stale >= 3 {
            self.st.rename_warned = true;
            tracing::warn!(
                "{stale} renames sent to {} were not confirmed by strip feedback. \
                 This LiveTrax build may not accept renames over OSC - check \
                 livetrax.rename_address, or set names.direction = \"daw-to-wing\" \
                 and drive names from the DAW instead.",
                self.cfg.livetrax.rename_address
            );
        }
        self.st
            .rename_watch
            .retain(|(_, _, at)| at.elapsed() <= Duration::from_secs(30));
    }

    fn propagate_worthy(&self, name: &str) -> bool {
        if name.is_empty() {
            return !self.cfg.names.skip_empty;
        }
        !self
            .cfg
            .names
            .ignore_prefixes
            .iter()
            .any(|p| !p.is_empty() && name.to_lowercase().starts_with(&p.to_lowercase()))
    }

    // ------------------------------------------------------------ timecode --

    /// The frame rate in force: the config's if it pins one, otherwise the
    /// session's, otherwise 30.
    fn fps(&self) -> Fps {
        self.cfg.timecode.fps.or(self.st.session_fps).unwrap_or_default()
    }

    fn sample_rate(&self) -> f64 {
        self.markers
            .sample_rate
            .unwrap_or(self.cfg.livetrax.sample_rate)
            .max(1.0)
    }

    /// Session start, in frames. A timecode in the config wins over the offset
    /// the session file carries.
    fn offset_frames(&self) -> i64 {
        if let Some(raw) = &self.cfg.timecode.offset {
            if let Some(tc) = timecode::Timecode::parse(raw) {
                return tc.to_frame_number(self.fps());
            }
        }
        (self.st.session_offset_samples as f64 / self.sample_rate() * self.fps().rate()).round()
            as i64
    }

    /// What the playhead reads. The DAW's own string when it is sending one,
    /// worked out from the sample position when it is not.
    fn now_timecode(&self) -> String {
        if let Some(tc) = &self.st.timecode {
            return tc.clone();
        }
        timecode::from_samples(
            self.st.position,
            self.sample_rate(),
            self.fps(),
            self.offset_frames(),
        )
        .to_string()
    }

    fn log_cue(&mut self, kind: CueKind, detail: impl Into<String>) {
        if !self.cfg.timecode.log {
            return;
        }
        let cue = Cue {
            timecode: self.now_timecode(),
            samples: self.st.position,
            kind,
            detail: detail.into(),
        };
        tracing::debug!("cue {} {} {}", cue.timecode, kind.label(), cue.detail);
        self.st.cues.push(cue);
        let limit = self.cfg.timecode.log_limit.max(1);
        if self.st.cues.len() > limit {
            let excess = self.st.cues.len() - limit;
            self.st.cues.drain(0..excess);
        }
    }

    /// The name to give a marker the bridge drops.
    fn marker_name(&mut self) -> Option<String> {
        let template = self.cfg.timecode.marker_template.trim();
        if template.is_empty() {
            return None;
        }
        self.st.markers_dropped += 1;
        Some(osc::template(
            template,
            &[
                ("tc", self.now_timecode()),
                ("samples", self.st.position.to_string()),
                ("n", self.st.markers_dropped.to_string()),
            ],
        ))
    }

    fn cues_as_csv(&self) -> String {
        let mut out = String::from("timecode,samples,kind,detail\n");
        for cue in &self.st.cues {
            // Detail is a free-text name; quote it and double any quotes in it.
            out.push_str(&format!(
                "{},{},{},\"{}\"\n",
                cue.timecode,
                cue.samples,
                cue.kind.label(),
                cue.detail.replace('"', "\"\"")
            ));
        }
        out
    }

    // ---------------------------------------------------------- live patch --

    /// Ask the console for its patch. Replies land in `self.live`.
    async fn start_live_query(&mut self, group: Option<String>) -> Result<()> {
        let group = group.unwrap_or_else(|| self.cfg.patch.output_group.clone());
        let Some(wing) = self.console.wing().cloned() else {
            tracing::warn!(
                "reading the output patch live is a WING conversation; a Qu has no \
                 equivalent, so the channel map comes from [map]"
            );
            return Ok(());
        };
        self.live = PatchModel::default();
        let asked = wing
            .query_patch(&self.cfg.patch.live, &group, self.cfg.channels())
            .await?;
        tracing::info!("asked the console about output group {group} ({asked} queries)");
        self.live_query = Some(LiveQuery {
            group,
            phase: 1,
            deadline: Instant::now() + Duration::from_millis(self.cfg.patch.live.settle_ms.max(200)),
            asked,
        });
        Ok(())
    }

    /// Move a query on when its settle window expires.
    async fn tick_live_query(&mut self) -> Result<()> {
        let Some(query) = &self.live_query else { return Ok(()) };
        if Instant::now() < query.deadline {
            return Ok(());
        }
        let group = query.group.clone();
        let phase = query.phase;
        let asked = query.asked;

        // Channel names are already tracked, so they need no queries of their own.
        self.live.channel_names = self
            .st
            .wing_names
            .iter()
            .filter(|(_, name)| !name.trim().is_empty())
            .map(|(ch, name)| (*ch, name.clone()))
            .collect();

        if phase == 1 {
            let wanted = self.live.unclaimed_sockets(&group);
            if !wanted.is_empty() {
                let Some(wing) = self.console.wing().cloned() else { return Ok(()) };
                let more = wing.query_input_names(&self.cfg.patch.live, &wanted).await?;
                self.live_query = Some(LiveQuery {
                    group,
                    phase: 2,
                    deadline: Instant::now()
                        + Duration::from_millis(self.cfg.patch.live.settle_ms.max(200)),
                    asked: asked + more,
                });
                return Ok(());
            }
        }

        self.live_query = None;
        self.apply_live_patch(&group);
        Ok(())
    }

    fn apply_live_patch(&mut self, group: &str) {
        let slots = self.live.slots(group);
        if slots.is_empty() {
            tracing::warn!(
                "the console did not answer any patch queries for {group}. Check \
                 [patch.live] addresses with `probe`, or use a .snap file instead."
            );
            self.patch = None;
            self.map = ChannelMap::build(&self.cfg.map, self.cfg.channels());
            self.publish();
            return;
        }
        let mapped = slots.iter().filter(|s| s.channel.is_some()).count();
        let info = PatchInfo {
            source: "console".into(),
            group: group.to_string(),
            outputs: slots.len(),
            mapped,
            slots: slots.clone(),
        };
        tracing::info!("patch: {}", info.summary());
        if self.cfg.patch.use_for_map {
            self.map = ChannelMap::from_outputs(&slots, self.cfg.map.strip_offset);
        }
        self.patch = Some(info);
        self.publish();
    }

    // ------------------------------------------------------------ commands --

    async fn on_command(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::QueryWingNames => {
                self.console.query_all_names(self.cfg.channels()).await?;
            }
            Command::RefreshDaw => {
                self.daw.set_surface().await?;
                self.daw.request_strip_list().await?;
            }
            Command::PushNamesToDaw => {
                let mut count = 0;
                for (ch, ssid) in self.map.pairs() {
                    let Some(name) = self.st.wing_names.get(&ch).cloned() else { continue };
                    if !self.propagate_worthy(&name) {
                        continue;
                    }
                    let target = format!(
                        "{}{}{}",
                        self.cfg.names.daw_prefix, name, self.cfg.names.daw_suffix
                    );
                    self.daw.rename_strip(ssid, &target).await?;
                    self.st.sent_daw.insert(ssid, (target.clone(), Instant::now()));
                    self.st.rename_watch.push((ssid, target, Instant::now()));
                    count += 1;
                }
                tracing::info!("pushed {count} console names to the DAW");
            }
            Command::PushNamesToWing => {
                let mut count = 0;
                for (ch, ssid) in self.map.pairs() {
                    let Some(name) = self.st.strips.get(&ssid).cloned() else { continue };
                    if !self.propagate_worthy(&name) {
                        continue;
                    }
                    let target = self.truncate_for_wing(&name);
                    self.console.set_name(ch, &target).await?;
                    self.st.sent_wing.insert(ch, (target, Instant::now()));
                    count += 1;
                }
                tracing::info!("pushed {count} track names to the console");
            }
            Command::Transport(action) => self.run_action(&action).await?,
            Command::RecallScene(idx) => {
                self.st.scene_echo = Some((idx, Instant::now()));
                self.console
                    .recall_scene(&self.cfg.scenes.recall_address, idx)
                    .await?;
            }
            Command::LocateMarker(name) => {
                self.run_action(&Action::LocateMarker(name)).await?;
            }
            Command::ReloadSession => self.load_session_markers(),
            Command::SetScenesEnabled(on) => {
                self.cfg.scenes.enabled = on;
                tracing::info!("scene linking {}", if on { "enabled" } else { "disabled" });
            }
            Command::CreateSession(req) => {
                if let Some(shared) = &self.shared {
                    if let Ok(mut s) = shared.lock() {
                        s.session_busy = true;
                        s.session_report = None;
                    }
                }
                let result = session::create(&req).map_err(|e| format!("{e:#}"));
                match &result {
                    Ok(report) => {
                        tracing::info!(
                            "created {} with {} tracks",
                            report.session_file.display(),
                            report.tracks
                        );
                        for w in &report.warnings {
                            tracing::warn!("{w}");
                        }
                        // Follow the new session so its markers are the live ones.
                        self.cfg.livetrax.session_file = Some(report.session_file.clone());
                        self.load_session_markers();
                    }
                    Err(e) => tracing::error!("create session: {e}"),
                }
                if let Some(shared) = &self.shared {
                    if let Ok(mut s) = shared.lock() {
                        s.session_busy = false;
                        s.session_report = Some(result);
                    }
                }
            }
            Command::ApplyChannelNames(entries) => {
                let count = entries.len();
                for (ch, name) in entries {
                    self.console.set_name(ch, &name).await?;
                    self.st.sent_wing.insert(ch, (name, Instant::now()));
                }
                tracing::info!("applied {count} names to the console");
            }
            Command::QueryLivePatch { output_group } => {
                self.cfg.patch.source = PatchSource::Console;
                if let Some(group) = &output_group {
                    self.cfg.patch.output_group = group.clone();
                }
                self.start_live_query(output_group).await?;
            }
            Command::SetPatch { snap_file, output_group } => {
                self.cfg.patch.source = PatchSource::Snap;
                self.cfg.patch.snap_file = snap_file;
                if let Some(group) = output_group {
                    self.cfg.patch.output_group = group;
                }
                self.load_patch();
                if self.cfg.patch.snap_file.is_none() || !self.cfg.patch.use_for_map {
                    self.map = ChannelMap::build(&self.cfg.map, self.cfg.channels());
                    tracing::info!("channel map back to [map]: {} pairs", self.map.len());
                }
            }
            Command::LocateTimecode(raw) => {
                let Some(tc) = timecode::Timecode::parse(&raw) else {
                    tracing::warn!("{raw:?} is not a timecode - try 01:02:03:04");
                    return Ok(());
                };
                match timecode::to_samples(
                    tc,
                    self.sample_rate(),
                    self.fps(),
                    self.offset_frames(),
                ) {
                    Some(samples) => {
                        tracing::info!("locate to {tc} ({samples} samples)");
                        self.daw.locate(samples, false).await?;
                    }
                    None => tracing::warn!("{tc} is before the start of the session"),
                }
            }
            Command::LocateSamples(samples) => {
                self.daw.locate(samples.max(0), false).await?;
            }
            Command::ExportCues(path) => {
                let csv = self.cues_as_csv();
                let count = self.st.cues.len();
                std::fs::write(&path, csv)
                    .with_context(|| format!("writing {}", path.display()))?;
                tracing::info!("wrote {count} cues to {}", path.display());
            }
            Command::ClearCues => {
                self.st.cues.clear();
                tracing::info!("cue log cleared");
            }
            Command::ApplyConfig(config) => self.apply_config(*config),
            Command::SaveConfig(path) => {
                let text = toml::to_string_pretty(&self.cfg).context("serialising config")?;
                std::fs::write(&path, text)
                    .with_context(|| format!("writing {}", path.display()))?;
                tracing::info!("wrote {} (comments are not preserved)", path.display());
            }
        }
        self.publish();
        Ok(())
    }

    /// Take a configuration from the preferences window. Everything except
    /// the sockets themselves can change while the bridge runs.
    fn apply_config(&mut self, new: Config) {
        let rebind = new.wing.host != self.cfg.wing.host
            || new.wing.port != self.cfg.wing.port
            || new.wing.local_port != self.cfg.wing.local_port
            || new.livetrax.host != self.cfg.livetrax.host
            || new.livetrax.port != self.cfg.livetrax.port
            || new.livetrax.local_port != self.cfg.livetrax.local_port;
        let session_changed = new.livetrax.session_file != self.cfg.livetrax.session_file;

        self.cfg = new;
        self.map = ChannelMap::build(&self.cfg.map, self.cfg.channels());
        self.load_patch();
        if session_changed {
            self.load_session_markers();
        }
        tracing::info!("configuration applied");
        if rebind {
            tracing::warn!(
                "hosts and ports change only on restart - the sockets are already bound"
            );
        }
        self.publish();
    }

    fn truncate_for_wing(&self, name: &str) -> String {
        let max = self.cfg.names.max_len_wing;
        if max > 0 && name.chars().count() > max {
            name.chars().take(max).collect()
        } else {
            name.to_string()
        }
    }

    /// Copy state out for the GUI. Cheap enough to run on the flush tick.
    fn publish(&self) {
        let Some(shared) = &self.shared else { return };
        let Ok(mut s) = shared.lock() else { return };
        s.wing_names = self.st.wing_names.clone();
        s.strips = self.st.strips.clone();
        s.pairs = self.map.pairs();
        s.markers = self.markers.iter().map(|m| (m.name.clone(), m.start)).collect();
        s.scene_map = self
            .cfg
            .scenes
            .map
            .iter()
            .map(|m| (m.scene, m.marker.clone()))
            .collect();
        s.sample_rate = self
            .markers
            .sample_rate
            .unwrap_or(self.cfg.livetrax.sample_rate);
        s.timecode = Some(self.now_timecode());
        s.fps = self.fps().label().to_string();
        s.fps_value = self.fps();
        s.tc_offset_frames = self.offset_frames();
        s.cues = self.st.cues.clone();
        s.playing = self.st.playing;
        s.recording = self.st.recording;
        s.looping = self.st.looping;
        s.punch_in = self.st.punch_in;
        s.punch_out = self.st.punch_out;
        s.click = self.st.click;
        s.speed = self.st.speed;
        s.session_end = self.st.session_end;
        s.console_events = self.st.console_events.clone();
        s.position = self.st.position;
        s.current_scene = self.st.current_scene;
        s.current_marker = self.st.current_marker.clone();
        s.last_wing_rx = self.st.last_wing_rx;
        s.last_daw_rx = self.st.last_daw_rx;
        s.wing_msgs = self.st.wing_msgs;
        s.daw_msgs = self.st.daw_msgs;
        s.names_enabled = self.cfg.names.enabled;
        s.names_direction = self.cfg.names.direction;
        s.scenes_enabled = self.cfg.scenes.enabled;
        s.wing_target = self.console.target();
        s.daw_target = self.daw.link.remote().to_string();
        s.session_file = self.cfg.livetrax.session_file.clone();
        s.channels = self.cfg.channels();
        s.patch_summary = self.patch.as_ref().map(PatchInfo::summary);
        s.patch_group = self.patch.as_ref().map(|p| p.group.clone());
        s.patch_source = self.patch.as_ref().map(|p| p.source.clone());
        s.patch_slots = self
            .patch
            .as_ref()
            .map(|p| p.slots.clone())
            .unwrap_or_default();
    }

    // -------------------------------------------------------------- session --

    fn session_path(&self) -> Option<PathBuf> {
        let raw = self.cfg.livetrax.session_file.as_ref()?;
        match markers::resolve_session_path(raw) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("session file: {e:#}");
                None
            }
        }
    }

    fn load_session_markers(&mut self) {
        let Some(path) = self.session_path() else { return };
        match markers::parse_session(&path) {
            Ok(info) => {
                let n = info.markers.len();
                self.markers.replace_from_file(info.markers, info.sample_rate);
                // Timecode follows the session unless the config pins it.
                self.st.session_fps = info.fps;
                self.st.session_offset_samples = info.offset_samples;
                tracing::info!(
                    "session {}: {n} markers, {} total, {} Hz, timecode {}",
                    path.display(),
                    self.markers.len(),
                    self.markers.sample_rate.unwrap_or_default(),
                    info.fps.map(|f| f.label()).unwrap_or("not set")
                );
            }
            Err(e) => tracing::warn!("parsing session: {e:#}"),
        }
    }

    /// Watch the session directory (LiveTrax saves by replacing the file, which
    /// would break a watch on the file itself).
    fn spawn_session_watcher(&self, tx: mpsc::Sender<()>) -> Option<notify::RecommendedWatcher> {
        if !self.cfg.livetrax.watch_session_file {
            return None;
        }
        let path = self.session_path()?;
        let dir = path.parent()?.to_path_buf();
        let target = path.clone();
        let mut last: Option<Instant> = None;
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !event.paths.contains(&target) {
                return;
            }
            // LiveTrax writes the file in several steps; coalesce them.
            if last.map(|t| t.elapsed() < Duration::from_millis(750)).unwrap_or(false) {
                return;
            }
            last = Some(Instant::now());
            let _ = tx.try_send(());
        });
        let mut watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("session watcher: {e}");
                return None;
            }
        };
        use notify::Watcher;
        if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
            tracing::warn!("session watcher on {}: {e}", dir.display());
            return None;
        }
        tracing::info!("watching {} for marker changes", dir.display());
        Some(watcher)
    }
}

/// Used by the `strips` subcommand.
pub async fn dump_strips(daw: &Daw, rx: &mut mpsc::Receiver<Incoming>, secs: u64) -> Result<()> {
    daw.set_surface().await.context("announcing surface")?;
    daw.request_strip_list().await.context("requesting strip list")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut strips: BTreeMap<u32, String> = BTreeMap::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(inc)) => {
                let args = &inc.msg.args;
                if matches!(inc.msg.addr.as_str(), "#reply" | "/reply") {
                    if let (Some(ssid), Some(name)) = (
                        args.get(1).and_then(osc::as_i64),
                        args.get(2).and_then(osc::as_str),
                    ) {
                        if args.first().and_then(osc::as_str) != Some("end_route_list") {
                            strips.insert(ssid as u32, name.to_string());
                        }
                    }
                }
            }
            _ => break,
        }
    }
    if strips.is_empty() {
        println!("No strips reported. Is the OSC surface enabled in LiveTrax, and is the port right?");
    } else {
        println!("{:>5}  name", "ssid");
        for (ssid, name) in &strips {
            println!("{ssid:>5}  {name}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::wing::Wing;
    use rosc::OscType;
    use std::net::UdpSocket as StdUdp;

    /// A bridge wired to two loopback sockets standing in for console and DAW.
    async fn harness() -> (Bridge, StdUdp, StdUdp) {
        let wing_peer = StdUdp::bind("127.0.0.1:0").unwrap();
        let daw_peer = StdUdp::bind("127.0.0.1:0").unwrap();
        wing_peer
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        daw_peer
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        let cfg: Config = toml::from_str(&format!(
            r#"
[wing]
host = "127.0.0.1"
port = {}
channels = 4
[livetrax]
host = "127.0.0.1"
port = {}
[names]
max_len_wing = 6
"#,
            wing_peer.local_addr().unwrap().port(),
            daw_peer.local_addr().unwrap().port()
        ))
        .unwrap();

        let local = "127.0.0.1:0".parse().unwrap();
        let (wing_link, _) = crate::osc::OscLink::bind("wing", local, wing_peer.local_addr().unwrap())
            .await
            .unwrap();
        let (daw_link, _) = crate::osc::OscLink::bind("daw", local, daw_peer.local_addr().unwrap())
            .await
            .unwrap();
        let bridge = Bridge::new(
            cfg.clone(),
            Console::Wing(Wing::new(wing_link, cfg.wing.clone())),
            Daw::new(daw_link, cfg.livetrax.clone()),
        );
        (bridge, wing_peer, daw_peer)
    }

    fn recv(sock: &StdUdp) -> Option<rosc::OscMessage> {
        let mut buf = [0u8; 4096];
        let (n, _) = sock.recv_from(&mut buf).ok()?;
        match rosc::decoder::decode_udp(&buf[..n]).ok()?.1 {
            rosc::OscPacket::Message(m) => Some(m),
            _ => None,
        }
    }

    #[tokio::test]
    async fn push_names_to_daw_renames_mapped_strips() {
        let (mut bridge, _wing, daw) = harness().await;
        bridge.st.wing_names.insert(1, "KICK".into());
        bridge.st.wing_names.insert(2, "Audio 7".into()); // placeholder, skipped

        bridge.on_command(Command::PushNamesToDaw).await.unwrap();

        let msg = recv(&daw).expect("a rename should have been sent");
        assert_eq!(msg.addr, "/strip/name");
        assert_eq!(msg.args[0], OscType::Int(1));
        assert_eq!(msg.args[1], OscType::String("KICK".into()));
        // The placeholder name must not produce a second message.
        assert!(recv(&daw).is_none());
    }

    #[tokio::test]
    async fn push_names_to_wing_truncates() {
        let (mut bridge, wing, _daw) = harness().await;
        bridge.st.strips.insert(1, "LEAD VOCAL".into());

        bridge.on_command(Command::PushNamesToWing).await.unwrap();

        let msg = recv(&wing).expect("a name should have been sent to the console");
        assert_eq!(msg.addr, "/ch/1/name");
        assert_eq!(msg.args[0], OscType::String("LEAD V".into()));
    }

    #[tokio::test]
    async fn transport_command_reaches_the_daw() {
        let (mut bridge, _wing, daw) = harness().await;
        bridge.on_command(Command::Transport(Action::Play)).await.unwrap();
        assert_eq!(recv(&daw).unwrap().addr, "/transport_play");
    }

    #[tokio::test]
    async fn create_session_publishes_a_report_and_is_adopted() {
        let (mut bridge, _wing, _daw) = harness().await;
        let shared = crate::shared::shared();
        bridge.shared = Some(shared.clone());

        let dir = std::env::temp_dir().join(format!(
            "wltb-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        bridge
            .on_command(Command::CreateSession(Box::new(session::SessionRequest {
                parent_dir: dir.clone(),
                name: "Unit".into(),
                sample_rate: 48_000,
                tracks: vec!["KICK".into(), "KICK".into()],
                template: None,
                connect_inputs: true,
                styles: Vec::new(),
                allow_minimal: true,
            })))
            .await
            .unwrap();

        let session_file = dir.join("Unit").join("Unit.ardour");
        assert!(session_file.is_file(), "session file should exist");
        let snap = shared.lock().unwrap();
        let report = snap.session_report.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(report.tracks, 2);
        assert!(!report.warnings.is_empty(), "the minimal path must warn");
        // The new session becomes the one the bridge follows for markers.
        assert_eq!(bridge.cfg.livetrax.session_file.as_ref(), Some(&session_file));
        // Duplicate track names are made unique before writing.
        let xml = std::fs::read_to_string(&session_file).unwrap();
        assert!(xml.contains("KICK 2"), "duplicate names should be numbered");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn applying_a_config_takes_effect_and_can_be_saved() {
        let (mut bridge, _wing, _daw) = harness().await;
        let mut updated = bridge.cfg.clone();
        updated.names.enabled = false;
        updated.map.strip_offset = 10;
        updated.timecode.marker_template = "take {n}".into();

        bridge.on_command(Command::ApplyConfig(Box::new(updated))).await.unwrap();
        assert!(!bridge.cfg.names.enabled);
        // The channel map is rebuilt, not left on the old offset.
        assert_eq!(bridge.map.strip(1), Some(11));

        let path = std::env::temp_dir().join(format!("wltb-cfg-{}.toml", std::process::id()));
        bridge.on_command(Command::SaveConfig(path.clone())).await.unwrap();
        let reloaded = Config::load(&path).unwrap();
        assert!(!reloaded.names.enabled);
        assert_eq!(reloaded.map.strip_offset, 10);
        assert_eq!(reloaded.timecode.marker_template, "take {n}");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn dropped_markers_are_named_and_logged_with_timecode() {
        let (mut bridge, _wing, daw) = harness().await;
        // A minute and a second in, at the default 48 kHz and 30 fps.
        bridge.st.position = 48_000 * 61;

        bridge.on_command(Command::Transport(Action::AddMarker)).await.unwrap();
        assert_eq!(recv(&daw).unwrap().addr, "/add_marker");

        let cue = bridge.st.cues.last().expect("the marker should be logged");
        assert_eq!(cue.kind, CueKind::Marker);
        assert_eq!(cue.timecode, "00:01:01:00");
        assert_eq!(cue.detail, "00:01:01:00", "the default template is the timecode");

        let csv = bridge.cues_as_csv();
        assert!(csv.starts_with("timecode,samples,kind,detail\n"));
        assert!(csv.contains("00:01:01:00,2928000,marker"), "{csv}");
    }

    #[tokio::test]
    async fn the_daws_own_timecode_wins_over_the_calculated_one() {
        let (mut bridge, _wing, _daw) = harness().await;
        bridge.st.position = 48_000 * 61;
        assert_eq!(bridge.now_timecode(), "00:01:01:00");

        // Once LiveTrax sends timecode, that is what gets stamped - it knows
        // about session offsets and pull-up that we would have to guess at.
        bridge
            .on_daw(Incoming {
                from: "127.0.0.1:1".parse().unwrap(),
                msg: rosc::OscMessage {
                    addr: "/position/smpte".into(),
                    args: vec![OscType::String("10:00:01:00".into())],
                },
            })
            .await
            .unwrap();
        assert_eq!(bridge.now_timecode(), "10:00:01:00");
    }

    #[tokio::test]
    async fn playback_actions_reach_the_daw() {
        let (mut bridge, _wing, daw) = harness().await;
        for (action, address) in [
            (Action::LoopToggle, "/loop_toggle"),
            (Action::PunchIn, "/toggle_punch_in"),
            (Action::PunchOut, "/toggle_punch_out"),
            (Action::ClickToggle, "/toggle_click"),
            (Action::Rewind, "/rewind"),
            (Action::FastForward, "/ffwd"),
            (Action::AllRecEnable, "/toggle_all_rec_enables"),
            (Action::JumpBars(-1.0), "/jump_bars"),
            (Action::JumpSeconds(10.0), "/jump_seconds"),
            (Action::SetSpeed(0.5), "/set_transport_speed"),
        ] {
            bridge.on_command(Command::Transport(action)).await.unwrap();
            assert_eq!(recv(&daw).expect("something should have been sent").addr, address);
        }
    }

    #[tokio::test]
    async fn playback_state_follows_the_daw_and_lights_the_console() {
        let (mut bridge, wing, _daw) = harness().await;
        bridge.cfg.transport.leds = vec![crate::config::LedMap {
            source: LedSource::Looping,
            address: "/$ctl/user/1/bu/5/led".into(),
            on: crate::config::Arg::Int(1),
            off: crate::config::Arg::Int(0),
        }];

        // The DAW says it is looping, whoever turned it on.
        bridge
            .on_daw(Incoming {
                from: "127.0.0.1:1".parse().unwrap(),
                msg: rosc::OscMessage {
                    addr: "/loop_toggle".into(),
                    args: vec![OscType::Int(1)],
                },
            })
            .await
            .unwrap();
        assert!(bridge.st.looping);

        let msg = recv(&wing).expect("the light should have followed");
        assert_eq!(msg.addr, "/$ctl/user/1/bu/5/led");
        assert_eq!(msg.args[0], OscType::Int(1));
    }

    #[tokio::test]
    async fn console_controls_are_remembered_for_learning() {
        let (mut bridge, _wing, _daw) = harness().await;
        let press = |id: &str, value: f32| ConsoleEvent::Control { id: id.into(), value };
        bridge.on_console(press("/$ctl/user/1/bu/7", 1.0)).await.unwrap();
        bridge.on_console(press("/$ctl/user/1/bu/7", 0.0)).await.unwrap();
        bridge.on_console(press("/$ctl/user/1/bu/8", 1.0)).await.unwrap();

        let events = &bridge.st.console_events;
        assert_eq!(events.len(), 2, "one entry per control, not per message");
        assert_eq!(events.last().unwrap().address, "/$ctl/user/1/bu/8");
        // The release updated the first entry rather than adding to it.
        assert_eq!(events[0].value, 0.0);
        assert!(events[1].seq > events[0].seq);

        // A channel name is not a control and must not clutter the list.
        bridge
            .on_console(ConsoleEvent::Name { channel: 1, name: "KICK".into() })
            .await
            .unwrap();
        assert_eq!(bridge.st.console_events.len(), 2);
        assert_eq!(bridge.st.wing_names.get(&1).map(String::as_str), Some("KICK"));
    }

    #[tokio::test]
    async fn locating_by_timecode_lands_on_the_right_sample() {
        let (mut bridge, _wing, daw) = harness().await;
        bridge
            .on_command(Command::LocateTimecode("00:00:10:00".into()))
            .await
            .unwrap();
        let msg = recv(&daw).expect("a locate should have been sent");
        assert_eq!(msg.addr, "/locate");
        assert_eq!(msg.args[0], OscType::Int(480_000), "ten seconds at 48 kHz");

        // Nonsense is refused rather than sent as zero.
        bridge.on_command(Command::LocateTimecode("half past four".into())).await.unwrap();
        assert!(recv(&daw).is_none());
    }
}
