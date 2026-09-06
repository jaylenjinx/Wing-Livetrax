//! State shared between the async bridge and the GUI thread.
//!
//! The bridge owns the truth and publishes a cheap snapshot; the GUI reads
//! snapshots and sends commands back. Nothing else crosses the boundary.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::{Action, Direction};
use crate::session::{SessionReport, SessionRequest};

pub type Shared = Arc<Mutex<Snapshot>>;

pub fn shared() -> Shared {
    Arc::new(Mutex::new(Snapshot::default()))
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// wing channel -> name as last reported by the console.
    pub wing_names: BTreeMap<u16, String>,
    /// ssid -> track name as last reported by the DAW.
    pub strips: BTreeMap<u32, String>,
    /// Resolved channel -> strip pairs.
    pub pairs: Vec<(u16, u32)>,
    pub markers: Vec<(String, i64)>,
    pub scene_map: Vec<(i32, String)>,
    pub sample_rate: f64,
    /// Playhead as the DAW reports it in SMPTE, when it is sending timecode.
    pub timecode: Option<String>,
    pub fps: String,
    /// Enough to put any sample position on the same clock as the header.
    pub fps_value: crate::timecode::Fps,
    pub tc_offset_frames: i64,
    pub cues: Vec<Cue>,
    pub playing: bool,
    pub recording: bool,
    pub looping: bool,
    pub punch_in: bool,
    pub punch_out: bool,
    pub click: bool,
    /// 1 is normal speed, 0 stopped, negatives backwards.
    pub speed: f32,
    /// Last sample the DAW reports for the session, for the scrub bar.
    pub session_end: i64,
    /// What the console has sent lately, so a button can be learned.
    pub console_events: Vec<ConsoleEvent>,
    pub position: i64,
    pub current_scene: Option<i32>,
    pub current_marker: Option<String>,
    pub last_wing_rx: Option<Instant>,
    pub last_daw_rx: Option<Instant>,
    pub wing_msgs: u64,
    pub daw_msgs: u64,
    pub names_enabled: bool,
    pub names_direction: Direction,
    pub scenes_enabled: bool,
    pub wing_target: String,
    pub daw_target: String,
    pub session_file: Option<PathBuf>,
    /// Console output patch in force, if any.
    pub patch_summary: Option<String>,
    pub patch_group: Option<String>,
    pub patch_source: Option<String>,
    /// The patch itself: output, what feeds it, and the resolved name.
    pub patch_slots: Vec<crate::patch::Slot>,
    pub channels: u16,
    /// Result of the most recent "create session" request.
    pub session_report: Option<Result<SessionReport, String>>,
    /// Set while a session is being written, so the GUI can show progress.
    pub session_busy: bool,
}

/// A message the console sent that was not a channel name: the raw material
/// for learning a button binding.
#[derive(Debug, Clone)]
pub struct ConsoleEvent {
    /// Rises with every message, so the interface can spot a new one.
    pub seq: u64,
    pub address: String,
    pub value: f32,
}

/// One line of the show log: what happened, and where.
#[derive(Debug, Clone)]
pub struct Cue {
    pub timecode: String,
    pub samples: i64,
    pub kind: CueKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueKind {
    Marker,
    Scene,
    TakeStart,
    TakeStop,
}

impl CueKind {
    pub fn label(self) -> &'static str {
        match self {
            CueKind::Marker => "marker",
            CueKind::Scene => "scene",
            CueKind::TakeStart => "roll",
            CueKind::TakeStop => "stop",
        }
    }
}

/// Requests from the GUI to the bridge.
#[derive(Debug, Clone)]
pub enum Command {
    /// Re-read every mirrored channel name from the console.
    QueryWingNames,
    /// Re-announce the surface and re-request the strip list.
    RefreshDaw,
    /// Push every known console name to the DAW (ignores the sync direction).
    PushNamesToDaw,
    /// Push every known DAW name to the console.
    PushNamesToWing,
    Transport(Action),
    /// Locate to a typed timecode, e.g. "01:02:03:04".
    LocateTimecode(String),
    /// Locate to a sample position, from the scrub bar.
    LocateSamples(i64),
    ExportCues(PathBuf),
    ClearCues,
    /// Replace the whole configuration, from the preferences window.
    ApplyConfig(Box<crate::config::Config>),
    RecallScene(i32),
    LocateMarker(String),
    ReloadSession,
    SetScenesEnabled(bool),
    /// Choose the console output the DAW records, and the snapshot to read it
    /// from. A `None` file drops back to the configured [map].
    SetPatch { snap_file: Option<PathBuf>, output_group: Option<String> },
    /// Read the patch from the console itself rather than a file.
    QueryLivePatch { output_group: Option<String> },
    CreateSession(Box<SessionRequest>),
    /// Write channel names straight to the console (channel, name).
    ApplyChannelNames(Vec<(u16, String)>),
    SaveConfig(PathBuf),
}

pub type CommandTx = tokio::sync::mpsc::UnboundedSender<Command>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<Command>;

pub fn command_channel() -> (CommandTx, CommandRx) {
    tokio::sync::mpsc::unbounded_channel()
}

// ------------------------------------------------------------------- log ---

const LOG_CAP: usize = 2_000;

/// A tracing writer that keeps the last few thousand lines for the GUI.
#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<VecDeque<String>>>);

impl LogBuffer {
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().map(|b| b.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut b) = self.0.lock() {
            b.clear();
        }
    }

    fn push(&self, line: String) {
        if let Ok(mut b) = self.0.lock() {
            if b.len() >= LOG_CAP {
                b.pop_front();
            }
            b.push_back(line);
        }
    }
}

pub struct LogWriter {
    buf: LogBuffer,
    pending: String,
}

impl io::Write for LogWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.pending.push_str(&String::from_utf8_lossy(data));
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        for line in self.pending.split('\n') {
            let line = line.trim_end();
            if !line.is_empty() {
                self.buf.push(line.to_string());
            }
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LogWriter { buf: self.clone(), pending: String::new() }
    }
}
