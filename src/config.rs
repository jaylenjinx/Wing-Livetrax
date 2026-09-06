//! Configuration model.
//!
//! Everything that touches a protocol detail (OSC address, argument type,
//! subscription command) lives here so it can be corrected in TOML without a
//! rebuild. The defaults are the best-known values; use `probe`/`learn` to
//! confirm them against your firmware.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub wing: Wing,
    pub livetrax: LiveTrax,
    #[serde(default)]
    pub names: Names,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default)]
    pub scenes: Scenes,
    #[serde(default)]
    pub map: Map,
    #[serde(default)]
    pub snapshot: Snapshot,
    #[serde(default)]
    pub patch: Patch,
    #[serde(default)]
    pub timecode: Timecode,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.wing.name_address.contains("{ch}"),
            "wing.name_address must contain the {{ch}} placeholder"
        );
        anyhow::ensure!(self.wing.channels > 0, "wing.channels must be > 0");
        Ok(())
    }
}

// ---------------------------------------------------------------- console ---

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Wing {
    /// Console IP address or hostname.
    pub host: String,
    /// WING OSC port (firmware default 2223).
    #[serde(default = "d_wing_port")]
    pub port: u16,
    /// Local UDP port to bind. 0 = ephemeral.
    #[serde(default)]
    pub local_port: u16,
    /// How many input channels to mirror.
    #[serde(default = "d_channels")]
    pub channels: u16,
    /// Address template for a channel name. `{ch}` is the 1-based channel.
    #[serde(default = "d_wing_name_addr")]
    pub name_address: String,
    /// Messages sent periodically to keep the console pushing changes to us.
    /// Sent with no arguments unless the entry carries them.
    #[serde(default = "d_wing_subscribe")]
    pub subscribe: Vec<String>,
    #[serde(default = "d_subscribe_ms")]
    pub subscribe_interval_ms: u64,
    /// Poll every mirrored channel name on this interval as a belt-and-braces
    /// fallback when subscriptions are not delivering. 0 disables polling.
    #[serde(default = "d_name_poll_ms")]
    pub name_poll_interval_ms: u64,
    /// Send an empty-argument query to read a value (X32/WING convention).
    #[serde(default = "d_true")]
    pub query_with_empty_args: bool,
}

fn d_wing_port() -> u16 { 2223 }
fn d_channels() -> u16 { 40 }
fn d_wing_name_addr() -> String { "/ch/{ch}/name".into() }
fn d_wing_subscribe() -> Vec<String> { vec!["/*S".into()] }
fn d_subscribe_ms() -> u64 { 5_000 }
fn d_name_poll_ms() -> u64 { 10_000 }
fn d_true() -> bool { true }

// ------------------------------------------------------------------- daw ---

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiveTrax {
    /// Machine running LiveTrax. Loopback when the bridge runs on the same box.
    #[serde(default = "d_daw_host")]
    pub host: String,
    /// LiveTrax / Ardour OSC port (default 3819).
    #[serde(default = "d_daw_port")]
    pub port: u16,
    /// Local UDP port to bind; a fixed port keeps firewall rules simple.
    #[serde(default = "d_daw_local_port")]
    pub local_port: u16,
    /// `/set_surface` bank size. 0 = one bank containing every strip.
    #[serde(default)]
    pub bank_size: u32,
    /// `/set_surface` strip-type bitmask. 3 = audio + midi tracks.
    #[serde(default = "d_strip_types")]
    pub strip_types: u32,
    /// `/set_surface` feedback bitmask.
    #[serde(default = "d_feedback")]
    pub feedback: u32,
    #[serde(default = "d_gainmode")]
    pub gain_mode: u32,
    /// Re-send `/set_surface` and re-request the strip list on this interval.
    #[serde(default = "d_refresh_ms")]
    pub refresh_interval_ms: u64,
    /// Address used to rename a strip. `{ssid}` is substituted when present;
    /// otherwise the ssid is sent as the first argument.
    #[serde(default = "d_rename_addr")]
    pub rename_address: String,
    /// Command used to drop a marker at the playhead.
    #[serde(default = "d_add_marker_addr")]
    pub add_marker_address: String,
    /// Send the marker name as an argument to `add_marker_address`.
    #[serde(default)]
    pub add_marker_takes_name: bool,
    /// Session file (`*.ardour`). Parsed for marker positions so scene recalls
    /// can locate the transport. Optional.
    #[serde(default)]
    pub session_file: Option<PathBuf>,
    /// Re-read the session file when it changes on disk.
    #[serde(default = "d_true")]
    pub watch_session_file: bool,
    /// Fallback sample rate used before the session/strip list reports one.
    #[serde(default = "d_sample_rate")]
    pub sample_rate: f64,
}

fn d_daw_host() -> String { "127.0.0.1".into() }
fn d_daw_port() -> u16 { 3819 }
fn d_daw_local_port() -> u16 { 3820 }
fn d_strip_types() -> u32 { 3 }
// 1 button status | 2 variable controls | 8 heartbeat | 64 timecode
// | 1024 playhead in samples
fn d_feedback() -> u32 { 1 | 2 | 8 | 64 | 1024 }
fn d_gainmode() -> u32 { 0 }
fn d_refresh_ms() -> u64 { 10_000 }
fn d_rename_addr() -> String { "/strip/name".into() }
fn d_add_marker_addr() -> String { "/add_marker".into() }
fn d_sample_rate() -> f64 { 48_000.0 }

// ----------------------------------------------------------------- names ---

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Names {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default)]
    pub direction: Direction,
    /// Hold a change for this long before pushing it, so a name being typed on
    /// the console does not generate a message per keystroke.
    #[serde(default = "d_debounce_ms")]
    pub debounce_ms: u64,
    /// Truncate names pushed to the console (scribble strips are narrow).
    #[serde(default = "d_max_len_wing")]
    pub max_len_wing: usize,
    /// Never overwrite a destination name with an empty string.
    #[serde(default = "d_true")]
    pub skip_empty: bool,
    /// Source names matching these (case-insensitive) prefixes are treated as
    /// "unnamed" and not propagated.
    #[serde(default = "d_placeholders")]
    pub ignore_prefixes: Vec<String>,
    /// Prepended/appended when pushing into the DAW, e.g. prefix = "W ".
    #[serde(default)]
    pub daw_prefix: String,
    #[serde(default)]
    pub daw_suffix: String,
}

impl Default for Names {
    fn default() -> Self {
        Self {
            enabled: true,
            direction: Direction::default(),
            debounce_ms: d_debounce_ms(),
            max_len_wing: d_max_len_wing(),
            skip_empty: true,
            ignore_prefixes: d_placeholders(),
            daw_prefix: String::new(),
            daw_suffix: String::new(),
        }
    }
}

fn d_debounce_ms() -> u64 { 300 }
fn d_max_len_wing() -> usize { 12 }
fn d_placeholders() -> Vec<String> {
    vec!["Audio ".into(), "MIDI ".into(), "Ch ".into(), "CH".into()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    #[default]
    WingToDaw,
    DawToWing,
    Bidirectional,
}

impl Direction {
    pub fn wing_to_daw(self) -> bool {
        matches!(self, Direction::WingToDaw | Direction::Bidirectional)
    }
    pub fn daw_to_wing(self) -> bool {
        matches!(self, Direction::DawToWing | Direction::Bidirectional)
    }
}

// ------------------------------------------------------------- transport ---

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Transport {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Console control -> DAW action.
    #[serde(default)]
    pub buttons: Vec<ButtonMap>,
    /// DAW state -> console LED / control feedback.
    #[serde(default)]
    pub leds: Vec<LedMap>,
    /// Console control -> per-track record arm.
    #[serde(default)]
    pub rec_arm: Vec<RecArmMap>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ButtonMap {
    /// Exact OSC address the console emits for this control.
    pub address: String,
    /// Fire only when the first argument is >= this value (default 0.5).
    /// Set to 0 to fire on every message regardless of value.
    #[serde(default = "d_threshold")]
    pub threshold: f32,
    pub action: Action,
}

fn d_threshold() -> f32 { 0.5 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecArmMap {
    pub address: String,
    #[serde(default = "d_threshold")]
    pub threshold: f32,
    /// DAW strip id to arm, or 0 to derive it from `map` using `channel`.
    #[serde(default)]
    pub strip: u32,
    #[serde(default)]
    pub channel: u16,
    /// true = latch to the incoming value, false = toggle on each press.
    #[serde(default = "d_true")]
    pub follow_value: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LedMap {
    /// What DAW state drives this control.
    pub source: LedSource,
    /// Console address to write.
    pub address: String,
    /// Value sent when the state is true / false.
    #[serde(default = "d_on")]
    pub on: Arg,
    #[serde(default = "d_off")]
    pub off: Arg,
}

fn d_on() -> Arg { Arg::Int(1) }
fn d_off() -> Arg { Arg::Int(0) }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LedSource {
    Playing,
    Recording,
    Stopped,
    Looping,
    PunchIn,
    PunchOut,
    Click,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Play,
    Stop,
    TogglePlay,
    RecordArmToggle,
    RecordStart,
    GotoStart,
    GotoEnd,
    NextMarker,
    PrevMarker,
    AddMarker,
    /// Wind forward and back. Pressing again on a rolling transport goes
    /// faster, the way the DAW's own buttons do.
    FastForward,
    Rewind,
    LoopToggle,
    PunchIn,
    PunchOut,
    ClickToggle,
    /// Arm or disarm every track at once.
    AllRecEnable,
    /// Panic: all notes off, everywhere.
    MidiPanic,
    /// Jump by whole bars, negative to go back.
    JumpBars(f32),
    /// Jump by seconds, negative to go back.
    JumpSeconds(f32),
    /// Play at a speed: 1 is normal, 0.5 half, -1 backwards, 0 stops.
    SetSpeed(f32),
    /// Locate to a named marker known from the session file or observed live.
    LocateMarker(String),
    /// Ardour/LiveTrax action path, e.g. "Transport/Record".
    #[allow(clippy::enum_variant_names)]
    AccessAction(String),
    /// Escape hatch: send an arbitrary OSC message to the DAW.
    Osc { address: String, #[serde(default)] args: Vec<Arg> },
}

// ---------------------------------------------------------------- scenes ---

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Scenes {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub direction: SceneDirection,
    /// Console address that reports the active scene/snapshot index.
    /// Confirm with `learn` — it differs between firmware revisions.
    #[serde(default = "d_scene_addr")]
    pub scene_address: String,
    /// Address used to recall a scene on the console (marker -> scene).
    #[serde(default = "d_scene_recall_addr")]
    pub recall_address: String,
    /// Roll the transport after locating to a scene's marker.
    #[serde(default)]
    pub locate_and_play: bool,
    /// While the transport is rolling, a scene recall drops a marker named
    /// after the scene instead of locating.
    #[serde(default = "d_true")]
    pub add_marker_while_rolling: bool,
    /// Act again when the console reports the scene that is already active.
    /// Off by default: consoles re-send current values on every subscription
    /// refresh, and acting on those would locate (or drop markers) on a timer.
    #[serde(default)]
    pub retrigger_same_scene: bool,
    /// When retriggering is on, ignore a repeat of the same scene that arrives
    /// within this window.
    #[serde(default = "d_retrigger_ms")]
    pub retrigger_guard_ms: u64,
    #[serde(default)]
    pub map: Vec<SceneMarker>,
}

impl Default for Scenes {
    fn default() -> Self {
        Self {
            enabled: false,
            direction: SceneDirection::default(),
            scene_address: d_scene_addr(),
            recall_address: d_scene_recall_addr(),
            locate_and_play: false,
            add_marker_while_rolling: true,
            retrigger_same_scene: false,
            retrigger_guard_ms: d_retrigger_ms(),
            map: Vec::new(),
        }
    }
}

fn d_retrigger_ms() -> u64 { 2_000 }
fn d_scene_addr() -> String { "/$ctl/lib/$actidx".into() }
fn d_scene_recall_addr() -> String { "/$ctl/lib/$action".into() }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SceneDirection {
    #[default]
    SceneToMarker,
    MarkerToScene,
    Bidirectional,
}

impl SceneDirection {
    pub fn scene_to_marker(self) -> bool {
        matches!(self, Self::SceneToMarker | Self::Bidirectional)
    }
    pub fn marker_to_scene(self) -> bool {
        matches!(self, Self::MarkerToScene | Self::Bidirectional)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SceneMarker {
    pub scene: i32,
    pub marker: String,
}

// ------------------------------------------------------------------- map ---

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Map {
    /// Straight offset mapping: wing channel N -> strip N + offset.
    #[serde(default = "d_true")]
    pub one_to_one: bool,
    #[serde(default)]
    pub strip_offset: i32,
    /// Explicit overrides / full manual map.
    #[serde(default)]
    pub pairs: Vec<Pair>,
}

impl Default for Map {
    fn default() -> Self {
        Self { one_to_one: true, strip_offset: 0, pairs: Vec::new() }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Pair {
    pub channel: u16,
    pub strip: u32,
}

/// Resolved, bidirectional channel <-> strip map.
#[derive(Debug, Clone, Default)]
pub struct ChannelMap {
    to_strip: BTreeMap<u16, u32>,
    to_channel: BTreeMap<u32, u16>,
}

impl ChannelMap {
    /// Map built from a console output patch: the channel feeding output N
    /// owns DAW strip N.
    pub fn from_outputs(slots: &[crate::patch::Slot], strip_offset: i32) -> Self {
        let mut out = ChannelMap::default();
        for slot in slots {
            let Some(channel) = slot.channel else { continue };
            let ssid = slot.output as i32 + strip_offset;
            if ssid >= 1 {
                out.insert(channel, ssid as u32);
            }
        }
        out
    }

    pub fn build(map: &Map, channels: u16) -> Self {
        let mut out = ChannelMap::default();
        if map.one_to_one {
            for ch in 1..=channels {
                let ssid = ch as i32 + map.strip_offset;
                if ssid >= 1 {
                    out.insert(ch, ssid as u32);
                }
            }
        }
        for p in &map.pairs {
            out.insert(p.channel, p.strip);
        }
        out
    }

    fn insert(&mut self, ch: u16, ssid: u32) {
        if let Some(old) = self.to_strip.insert(ch, ssid) {
            self.to_channel.remove(&old);
        }
        self.to_channel.insert(ssid, ch);
    }

    pub fn strip(&self, ch: u16) -> Option<u32> { self.to_strip.get(&ch).copied() }
    pub fn channel(&self, ssid: u32) -> Option<u16> { self.to_channel.get(&ssid).copied() }
    pub fn pairs(&self) -> Vec<(u16, u32)> {
        self.to_strip.iter().map(|(c, s)| (*c, *s)).collect()
    }
    pub fn len(&self) -> usize { self.to_strip.len() }
}

// -------------------------------------------------------------- timecode ---

/// SMPTE timecode: how to read it, and what to stamp with it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Timecode {
    /// Frame rate. Left unset, it follows the session file.
    #[serde(default)]
    pub fps: Option<crate::timecode::Fps>,
    /// Session start, when you want one the session file does not carry.
    #[serde(default)]
    pub offset: Option<String>,
    /// Name for markers the bridge drops. `{tc}` is the timecode, `{samples}`
    /// the playhead, `{n}` a running count.
    #[serde(default = "d_marker_template")]
    pub marker_template: String,
    /// Keep a timestamped log of markers, scene recalls and takes.
    #[serde(default = "d_true")]
    pub log: bool,
    /// Log transport starts and stops as well.
    #[serde(default)]
    pub log_transport: bool,
    #[serde(default = "d_log_limit")]
    pub log_limit: usize,
}

impl Default for Timecode {
    fn default() -> Self {
        Self {
            fps: None,
            offset: None,
            marker_template: d_marker_template(),
            log: true,
            log_transport: false,
            log_limit: d_log_limit(),
        }
    }
}

fn d_marker_template() -> String { "{tc}".into() }
fn d_log_limit() -> usize { 2_000 }

// ----------------------------------------------------------------- patch ---

/// Which console output feeds the DAW.
///
/// The console records through one port group, and that group's output patch
/// decides which source lands on which track. It is rarely channel 1 to
/// track 1, so pointing this at a `.snap` saved from the console makes the map
/// follow the desk's own patching.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Patch {
    /// Where the patch comes from: a saved `.snap`, or the console itself.
    #[serde(default)]
    pub source: PatchSource,
    /// WING `.snap` file to read the patch from.
    #[serde(default)]
    pub snap_file: Option<PathBuf>,
    /// Port group the DAW records: USB, CRD, A, B, C, SC, MOD, REC, AES, LCL.
    #[serde(default = "d_output_group")]
    pub output_group: String,
    /// Build the channel <-> strip map from the patch instead of [map].
    #[serde(default = "d_true")]
    pub use_for_map: bool,
    /// Ask the console for its patch when the bridge starts.
    #[serde(default = "d_true")]
    pub query_on_start: bool,
    #[serde(default)]
    pub live: LivePatch,
}

impl Default for Patch {
    fn default() -> Self {
        Self {
            source: PatchSource::default(),
            snap_file: None,
            output_group: d_output_group(),
            use_for_map: true,
            query_on_start: true,
            live: LivePatch::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PatchSource {
    /// Read the patch from `snap_file`.
    #[default]
    Snap,
    /// Query the console over OSC.
    Console,
}

fn d_output_group() -> String { "USB".into() }

/// Addresses used to read the patch from a running console.
///
/// A `.snap` is the node tree serialised to JSON, so the OSC addresses are the
/// same paths with `ae_data.` dropped and dots turned into slashes - which is
/// why `ae_data.ch.1.name` is the already-known `/ch/1/name`. Confirm the rest
/// with `probe` if your firmware differs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LivePatch {
    /// `{grp}` = port group, `{n}` = output number.
    #[serde(default = "d_out_group_addr")]
    pub out_source_group: String,
    #[serde(default = "d_out_index_addr")]
    pub out_source_index: String,
    /// `{ch}` = channel number.
    #[serde(default = "d_ch_group_addr")]
    pub channel_input_group: String,
    #[serde(default = "d_ch_index_addr")]
    pub channel_input_index: String,
    /// `{sect}` = aux/bus/main/mtx/fx, `{n}` = object number.
    #[serde(default = "d_object_name_addr")]
    pub object_name: String,
    /// `{grp}` = port group, `{n}` = input number.
    #[serde(default = "d_input_name_addr")]
    pub input_name: String,
    /// How long to collect replies before building the map.
    #[serde(default = "d_settle_ms")]
    pub settle_ms: u64,
    /// Outputs per port group, so the bridge knows how many to ask about.
    #[serde(default = "d_group_sizes")]
    pub group_sizes: BTreeMap<String, u16>,
}

impl Default for LivePatch {
    fn default() -> Self {
        Self {
            out_source_group: d_out_group_addr(),
            out_source_index: d_out_index_addr(),
            channel_input_group: d_ch_group_addr(),
            channel_input_index: d_ch_index_addr(),
            object_name: d_object_name_addr(),
            input_name: d_input_name_addr(),
            settle_ms: d_settle_ms(),
            group_sizes: d_group_sizes(),
        }
    }
}

fn d_out_group_addr() -> String { "/io/out/{grp}/{n}/grp".into() }
fn d_out_index_addr() -> String { "/io/out/{grp}/{n}/in".into() }
fn d_ch_group_addr() -> String { "/ch/{ch}/in/conn/grp".into() }
fn d_ch_index_addr() -> String { "/ch/{ch}/in/conn/in".into() }
fn d_object_name_addr() -> String { "/{sect}/{n}/name".into() }
fn d_input_name_addr() -> String { "/io/in/{grp}/{n}/name".into() }
fn d_settle_ms() -> u64 { 1_500 }

/// Output counts per port group on a WING.
fn d_group_sizes() -> BTreeMap<String, u16> {
    [
        ("LCL", 8u16), ("AUX", 8), ("A", 48), ("B", 48), ("C", 48),
        ("SC", 32), ("USB", 48), ("CRD", 64), ("MOD", 64), ("REC", 4), ("AES", 2),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

// -------------------------------------------------------------- snapshot ---

/// Offline WING snapshot export.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Snapshot {
    /// Line written per channel. `{path}` is the channel's name address,
    /// `{name}` the quoted name, `{raw}` the unquoted one, `{ch}` the channel.
    #[serde(default = "d_snap_line")]
    pub line: String,
    /// Address forms to look for when rewriting a console export. The live
    /// `wing.name_address` is always tried as well.
    #[serde(default = "d_name_paths")]
    pub name_paths: Vec<String>,
    #[serde(default = "d_true")]
    pub include_comments: bool,
    /// Console channel the first track maps to.
    #[serde(default = "d_first_channel")]
    pub first_channel: u16,
    #[serde(default)]
    pub include_busses: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            line: d_snap_line(),
            name_paths: d_name_paths(),
            include_comments: true,
            first_channel: 1,
            include_busses: false,
        }
    }
}

fn d_snap_line() -> String { "{path} {name}".into() }
fn d_first_channel() -> u16 { 1 }
fn d_name_paths() -> Vec<String> {
    vec!["/ch/{ch}/name".into(), "ch.{ch}/name".into(), "/ch.{ch}/name".into()]
}

// ------------------------------------------------------------------ args ---

/// A literal OSC argument written in TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Arg {
    Bool(bool),
    Int(i32),
    Float(f32),
    Str(String),
}

impl Arg {
    pub fn to_osc(&self) -> rosc::OscType {
        match self {
            Arg::Bool(b) => rosc::OscType::Bool(*b),
            Arg::Int(i) => rosc::OscType::Int(*i),
            Arg::Float(f) => rosc::OscType::Float(*f),
            Arg::Str(s) => rosc::OscType::String(s.clone()),
        }
    }
}
