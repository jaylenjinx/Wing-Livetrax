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
use crate::shared::{Command, CommandRx, Shared};
use crate::patch::PatchModel;
use crate::snapfile::SnapFile;
use crate::wing::Wing;

/// How long a value we pushed is ignored when it comes straight back.
const ECHO_WINDOW: Duration = Duration::from_millis(1_500);
/// How long after a scene-driven locate a marker report is ignored.
const SCENE_LOOP_WINDOW: Duration = Duration::from_millis(1_500);

pub struct Bridge {
    cfg: Config,
    wing: Wing,
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
    /// Output number -> name, for naming new sessions.
    names: Vec<(u16, String)>,
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
    position: i64,
    current_scene: Option<i32>,
    /// Scene we last acted on, for repeat rate-limiting.
    last_scene_act: Option<(i32, Instant)>,
    /// Scene we pushed to the console, so its confirmation is not acted on.
    scene_echo: Option<(i32, Instant)>,
    current_marker: Option<String>,
    last_scene_locate: Option<Instant>,
    strip_list_seen: bool,
    last_wing_rx: Option<Instant>,
    last_daw_rx: Option<Instant>,
    wing_msgs: u64,
    daw_msgs: u64,
}

impl Bridge {
    pub fn new(cfg: Config, wing: Wing, daw: Daw) -> Self {
        let map = ChannelMap::build(&cfg.map, cfg.wing.channels);
        let mut markers = MarkerTable::default();
        markers.sample_rate = Some(cfg.livetrax.sample_rate);
        let mut bridge = Self {
            cfg,
            wing,
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
                    names: slots.iter().map(|s| (s.output, s.name.clone())).collect(),
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
        mut wing_rx: mpsc::Receiver<Incoming>,
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
        self.wing.subscribe().await.ok();
        if self.cfg.patch.source == PatchSource::Console && self.cfg.patch.query_on_start {
            // Names first: the patch resolver uses them.
            self.wing.query_all_names().await.ok();
            if let Err(e) = self.start_live_query(None).await {
                tracing::warn!("patch query: {e:#}");
            }
        }
        self.daw.set_surface().await.ok();
        self.daw.request_strip_list().await.ok();
        if self.cfg.names.enabled {
            self.wing.query_all_names().await.ok();
        }

        let mut subscribe = tokio::time::interval(Duration::from_millis(
            self.cfg.wing.subscribe_interval_ms.max(500),
        ));
        let mut refresh = tokio::time::interval(Duration::from_millis(
            self.cfg.livetrax.refresh_interval_ms.max(1_000),
        ));
        let poll_ms = self.cfg.wing.name_poll_interval_ms;
        let names_on = self.cfg.names.enabled;
        let mut name_poll = tokio::time::interval(Duration::from_millis(poll_ms.max(1_000)));
        let mut flush = tokio::time::interval(Duration::from_millis(50));

        loop {
            tokio::select! {
                Some(inc) = wing_rx.recv() => {
                    if let Err(e) = self.on_wing(inc).await {
                        tracing::warn!("wing event: {e:#}");
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
                _ = subscribe.tick() => { self.wing.subscribe().await.ok(); }
                _ = refresh.tick() => {
                    self.daw.set_surface().await.ok();
                    self.daw.request_strip_list().await.ok();
                }
                _ = name_poll.tick(), if poll_ms > 0 && names_on => {
                    self.wing.query_all_names().await.ok();
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

    async fn on_wing(&mut self, inc: Incoming) -> Result<()> {
        self.st.last_wing_rx = Some(Instant::now());
        self.st.wing_msgs += 1;
        let addr = inc.msg.addr.as_str();
        tracing::trace!("wing -> {}", osc::render(&inc.msg));

        if let Some(ch) = self.wing.channel_of_name_address(addr) {
            if let Some(name) = inc.msg.args.first().and_then(osc::as_str) {
                // Names are always tracked - the GUI and session generator need
                // them even when name sync is off or pointed the other way.
                self.on_wing_name(ch, name.trim());
                return Ok(());
            }
        }

        let value = inc
            .msg
            .args
            .first()
            .and_then(osc::as_f32)
            .unwrap_or(1.0);

        // Patch replies only matter while a query is outstanding.
        if self.live_query.is_some()
            && crate::patch::absorb(&mut self.live, &self.cfg.patch.live, addr, &inc.msg.args)
        {
            return Ok(());
        }

        if self.cfg.transport.enabled {
            // Cloned so the borrow of self.cfg ends before the action runs.
            let hits: Vec<Action> = self
                .cfg
                .transport
                .buttons
                .iter()
                .filter(|b| b.address == addr && (b.threshold <= 0.0 || value >= b.threshold))
                .map(|b| b.action.clone())
                .collect();
            for action in hits {
                tracing::info!("wing {addr} -> {action:?}");
                self.run_action(&action).await?;
            }

            let arms: Vec<RecArmMap> = self
                .cfg
                .transport
                .rec_arm
                .iter()
                .filter(|r| r.address == addr)
                .cloned()
                .collect();
            for arm in arms {
                let ssid = if arm.strip > 0 {
                    arm.strip
                } else {
                    self.map.strip(arm.channel).unwrap_or(0)
                };
                if ssid == 0 {
                    tracing::warn!("rec_arm {addr}: no strip for channel {}", arm.channel);
                    continue;
                }
                let on = if arm.follow_value { value >= arm.threshold } else { true };
                tracing::info!("wing {addr} -> strip {ssid} rec {}", on as i32);
                self.daw.rec_enable_strip(ssid, on).await?;
            }
        }

        if self.cfg.scenes.enabled
            && self.cfg.scenes.direction.scene_to_marker()
            && addr == self.cfg.scenes.scene_address
        {
            if let Some(idx) = inc.msg.args.first().and_then(osc::as_i64) {
                self.on_scene(idx as i32).await?;
            }
        }
        Ok(())
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
            "/rec_enable_toggle" | "/record_tally" | "/transport_record" => {
                let on = args.first().and_then(osc::as_f32).unwrap_or(0.0) > 0.5;
                self.set_recording(on).await?;
            }
            "/position/samples" => {
                if let Some(p) = args.first().and_then(osc::as_i64) {
                    self.st.position = p;
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
        self.wing
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
            };
            let arg = if on { led.on.to_osc() } else { led.off.to_osc() };
            self.wing.send_raw(&led.address, vec![arg]).await?;
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
            Action::AddMarker => self.daw.add_marker(None).await?,
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
            self.wing.set_name(ch, &name).await?;
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

    // ---------------------------------------------------------- live patch --

    /// Ask the console for its patch. Replies land in `self.live`.
    async fn start_live_query(&mut self, group: Option<String>) -> Result<()> {
        let group = group.unwrap_or_else(|| self.cfg.patch.output_group.clone());
        self.live = PatchModel::default();
        let asked = self
            .wing
            .query_patch(&self.cfg.patch.live, &group, self.cfg.wing.channels)
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
                let more = self
                    .wing
                    .query_input_names(&self.cfg.patch.live, &wanted)
                    .await?;
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
            self.map = ChannelMap::build(&self.cfg.map, self.cfg.wing.channels);
            self.publish();
            return;
        }
        let mapped = slots.iter().filter(|s| s.channel.is_some()).count();
        let info = PatchInfo {
            source: "console".into(),
            group: group.to_string(),
            outputs: slots.len(),
            mapped,
            names: slots.iter().map(|s| (s.output, s.name.clone())).collect(),
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
                self.wing.query_all_names().await?;
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
                    self.wing.set_name(ch, &target).await?;
                    self.st.sent_wing.insert(ch, (target, Instant::now()));
                    count += 1;
                }
                tracing::info!("pushed {count} track names to the console");
            }
            Command::Transport(action) => self.run_action(&action).await?,
            Command::RecallScene(idx) => {
                self.st.scene_echo = Some((idx, Instant::now()));
                self.wing
                    .recall_scene(&self.cfg.scenes.recall_address, idx)
                    .await?;
            }
            Command::LocateMarker(name) => {
                self.run_action(&Action::LocateMarker(name)).await?;
            }
            Command::ReloadSession => self.load_session_markers(),
            Command::SetNamesEnabled(on) => {
                self.cfg.names.enabled = on;
                tracing::info!("name sync {}", if on { "enabled" } else { "disabled" });
            }
            Command::SetNamesDirection(dir) => {
                self.cfg.names.direction = dir;
                tracing::info!("name sync direction: {dir:?}");
            }
            Command::SetScenesEnabled(on) => {
                self.cfg.scenes.enabled = on;
                tracing::info!("scene linking {}", if on { "enabled" } else { "disabled" });
            }
            Command::SetSessionFile(path) => {
                tracing::info!("session file: {}", path.display());
                self.cfg.livetrax.session_file = Some(path);
                self.load_session_markers();
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
                    self.wing.set_name(ch, &name).await?;
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
                    self.map = ChannelMap::build(&self.cfg.map, self.cfg.wing.channels);
                    tracing::info!("channel map back to [map]: {} pairs", self.map.len());
                }
            }
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
        s.playing = self.st.playing;
        s.recording = self.st.recording;
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
        s.wing_target = self.wing.link.remote().to_string();
        s.daw_target = self.daw.link.remote().to_string();
        s.session_file = self.cfg.livetrax.session_file.clone();
        s.channels = self.cfg.wing.channels;
        s.patch_summary = self.patch.as_ref().map(PatchInfo::summary);
        s.patch_group = self.patch.as_ref().map(|p| p.group.clone());
        s.patch_source = self.patch.as_ref().map(|p| p.source.clone());
        s.patch_names = self
            .patch
            .as_ref()
            .map(|p| p.names.clone())
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
            Ok((fresh, sr)) => {
                let n = fresh.len();
                self.markers.replace_from_file(fresh, sr);
                tracing::info!(
                    "session {}: {n} markers, {} total, sample rate {}",
                    path.display(),
                    self.markers.len(),
                    self.markers.sample_rate.unwrap_or_default()
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
            Wing::new(wing_link, cfg.wing.clone()),
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
    async fn save_config_round_trips() {
        let (mut bridge, _wing, _daw) = harness().await;
        let path = std::env::temp_dir().join(format!(
            "wltb-cfg-{}.toml",
            std::process::id()
        ));
        bridge.on_command(Command::SetNamesEnabled(false)).await.unwrap();
        bridge.on_command(Command::SaveConfig(path.clone())).await.unwrap();

        let reloaded = Config::load(&path).unwrap();
        assert!(!reloaded.names.enabled, "the toggle should survive a save");
        assert_eq!(reloaded.wing.channels, 4);
        std::fs::remove_file(&path).ok();
    }
}
