//! Turn a patch sheet into a console snapshot and a DAW track list.
//!
//! The sheet is an **overlay**, not a whole console: it is applied on top of a
//! base `.snap` and only touches the nodes its filled-in cells name. The base
//! is the factory snapshot unless you pass your own, so a sheet can be used
//! either to lay out a fresh desk or to re-patch a show file whose effects,
//! EQ and bus structure should survive untouched.
//!
//! Two rules keep the result loadable:
//!
//! * **nothing is invented.** A node that is not already in the base is not
//!   created - it is reported instead. A WING snapshot is a fixed tree and a
//!   node this tool made up would at best be ignored and at worst refused.
//! * **types are the console's.** A whole number is written as an integer and
//!   a switch as a boolean, which is what the desk and WING-Edit both write.
//!
//! The preamp is the one place where the sheet's shape and the console's differ.
//! Gain, phantom and polarity belong to the **socket**, not the channel, so
//! they are written by following the channel's input patch to `io/in/<GRP>/<n>`
//! - which means the Source column has to be right for the Gain column to land.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::sheet::{Row, Sheet, SourceRef};

/// The factory snapshot, used when no base is given. It is a complete console
/// tree, so every node the builder wants to write already exists in it.
const FACTORY: &str = include_str!("../assets/wing-compact.snap");

/// Which desk the sheet describes.
///
/// A WING has a documented snapshot format, so a sheet can be turned into a
/// console file as well as a session. A Qu has no such thing in public, so for
/// those the sheet builds the session and nothing else - which is all it is
/// asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Desk {
    #[default]
    Wing,
    Qu16,
    Qu24,
    Qu32,
}

impl Desk {
    pub const ALL: [Desk; 4] = [Desk::Wing, Desk::Qu16, Desk::Qu24, Desk::Qu32];

    pub fn label(self) -> &'static str {
        match self {
            Desk::Wing => "Behringer WING",
            Desk::Qu16 => "Allen & Heath Qu-16",
            Desk::Qu24 => "Allen & Heath Qu-24",
            Desk::Qu32 => "Allen & Heath Qu-32",
        }
    }

    /// "a WING", "an Allen & Heath Qu-16".
    pub fn article(self) -> &'static str {
        match self {
            Desk::Wing => "a",
            Desk::Qu16 | Desk::Qu24 | Desk::Qu32 => "an",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Desk::Wing => "wing",
            Desk::Qu16 => "qu-16",
            Desk::Qu24 => "qu-24",
            Desk::Qu32 => "qu-32",
        }
    }

    /// Mono input channels, where the desk has a fixed number.
    pub fn inputs(self) -> Option<u16> {
        match self {
            Desk::Wing => None,
            Desk::Qu16 => Some(16),
            Desk::Qu24 => Some(24),
            Desk::Qu32 => Some(32),
        }
    }

    /// Whether a console file can be written for it.
    pub fn writes_snapshot(self) -> bool {
        matches!(self, Desk::Wing)
    }

    pub fn from_name(raw: &str) -> Option<Desk> {
        let folded: String = raw
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        Some(match folded.as_str() {
            "wing" | "wingcompact" | "wingrack" | "wingfull" => Desk::Wing,
            "qu16" => Desk::Qu16,
            "qu24" => Desk::Qu24,
            "qu32" => Desk::Qu32,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// The desk the sheet is for.
    pub desk: Desk,
    /// Base snapshot to overlay the sheet onto. `None` uses the factory tree.
    pub base: Option<PathBuf>,
    /// Port group the DAW records from, e.g. `USB`.
    pub record_group: String,
    /// Copy each channel's name, colour and icon onto the socket it takes.
    pub label_sources: bool,
    /// Leave outputs of the record group that the sheet does not list alone,
    /// instead of switching them off.
    pub keep_unlisted_outputs: bool,
}

impl Default for BuildRequest {
    fn default() -> Self {
        Self {
            desk: Desk::default(),
            base: None,
            record_group: "USB".into(),
            label_sources: true,
            keep_unlisted_outputs: false,
        }
    }
}

/// One node the sheet moved.
#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub from: Value,
    pub to: Value,
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:<32} {} -> {}", self.path, brief(&self.from), brief(&self.to))
    }
}

fn brief(v: &Value) -> String {
    match v {
        Value::String(s) if s.is_empty() => "\"\"".into(),
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}

/// One DAW track: where it sits, and what it is called.
#[derive(Debug, Clone)]
pub struct Track {
    /// Output of the record group, which is also the track's position.
    pub output: u16,
    pub name: String,
    /// Console channel behind it, when the sheet named one.
    pub channel: Option<u16>,
    /// Console colour index, for the track colour.
    pub colour: Option<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub changes: Vec<Change>,
    pub warnings: Vec<String>,
    /// Channels the sheet touched.
    pub channels: Vec<u16>,
    pub tracks: Vec<Track>,
    /// Tracks with no channel behind them, added to keep track N on output N.
    pub filler: usize,
    /// Outputs of the record group switched off because the sheet omitted them.
    pub cleared: usize,
    pub base: Option<PathBuf>,
    pub desk: Desk,
}

#[derive(Debug)]
pub struct Built {
    /// The console file, for desks that have one.
    pub snapshot: Option<Value>,
    pub report: Report,
}

impl Built {
    pub fn write_snap(&self, path: &Path) -> Result<()> {
        let Some(snapshot) = &self.snapshot else {
            bail!(
                "there is no console file to write for {} {} - the sheet builds the session only",
                self.report.desk.article(),
                self.report.desk.label()
            );
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).ok();
        }
        let text = serde_json::to_string(snapshot).context("serialising the snapshot")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

// ------------------------------------------------------------------ build ---

pub fn build(sheet: &Sheet, req: &BuildRequest) -> Result<Built> {
    if !req.desk.writes_snapshot() {
        return build_tracks_only(sheet, req);
    }
    let text = match &req.base {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading base snapshot {}", path.display()))?,
        None => FACTORY.to_string(),
    };
    let mut root: Value = serde_json::from_str(&text).with_context(|| match &req.base {
        Some(p) => format!("{} is not a WING .snap (JSON) file", p.display()),
        None => "the built-in factory snapshot is not valid JSON".into(),
    })?;
    if root.get("ae_data").is_none() {
        bail!(
            "{} has no ae_data section - is it a WING snapshot?",
            req.base.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        );
    }

    let mut w = Writer { root: &mut root, report: Report::default() };
    w.report.desk = req.desk;
    w.report.base = req.base.clone();
    w.report.warnings.extend(sheet.warnings.iter().cloned());

    let group = req.record_group.to_uppercase();
    if !w.exists(&format!("ae_data/io/out/{group}")) {
        bail!(
            "the base snapshot has no {group} output group; it has {}",
            w.output_groups().join(", ")
        );
    }

    for row in &sheet.rows {
        w.channel(row, req);
    }
    w.record_patch(sheet, &group, req);
    w.stamp();

    let report = w.report;
    Ok(Built { snapshot: Some(root), report })
}

/// A desk with no snapshot format: read the sheet, build the track list, and
/// say plainly which columns had nowhere to go.
fn build_tracks_only(sheet: &Sheet, req: &BuildRequest) -> Result<Built> {
    let mut report = Report { desk: req.desk, ..Report::default() };
    report.warnings.extend(sheet.warnings.iter().cloned());
    report.channels = sheet.rows.iter().map(|r| r.channel).collect();

    if let Some(inputs) = req.desk.inputs() {
        for row in sheet.rows.iter().filter(|r| r.channel > inputs) {
            report.warnings.push(format!(
                "line {}: channel {} is past the {inputs} inputs of {} {}",
                row.line,
                row.channel,
                req.desk.article(),
                req.desk.label()
            ));
        }
    }

    let unusable = unusable_columns(sheet);
    if !unusable.is_empty() {
        report.warnings.push(format!(
            "{} {} has no console file to write, so these columns were read but not applied: {}",
            req.desk.article(),
            req.desk.label(),
            unusable.join(", ")
        ));
    }

    report.tracks = track_list(sheet, &mut report);
    Ok(Built { snapshot: None, report })
}

/// Columns that only mean something when a console file is being written.
fn unusable_columns(sheet: &Sheet) -> Vec<&'static str> {
    // Checked column by column so they are reported in the order the sheet's
    // own column reference lists them, not the order the rows happened to
    // fill them in.
    type Present = fn(&Row) -> bool;
    let columns: [(&'static str, Present); 14] = [
        ("Source", |r| r.source.is_some()),
        ("Gain", |r| r.gain.is_some()),
        ("48V", |r| r.phantom.is_some()),
        ("Polarity", |r| r.polarity.is_some()),
        ("Low Cut", |r| r.low_cut.is_some()),
        ("Icon", |r| r.icon.is_some()),
        ("DCA", |r| r.dca.is_some()),
        ("Mute Group", |r| r.mute_group.is_some()),
        ("Fader", |r| r.fader.is_some()),
        ("Pan", |r| r.pan.is_some()),
        ("Main", |r| r.main.is_some()),
        ("Mute", |r| r.mute.is_some()),
        ("Sends", |r| r.sends.is_some()),
        ("Link", |r| r.link.is_some()),
    ];
    columns
        .iter()
        .filter(|(_, used)| sheet.rows.iter().any(used))
        .map(|(label, _)| *label)
        .collect()
}

/// Which row records to which track.
///
/// A sheet with no Track column at all is not a sheet without tracks: on a desk
/// that records its inputs in order, track N is channel N, and saying so beats
/// producing nothing.
fn track_map(sheet: &Sheet) -> (BTreeMap<u16, &Row>, bool) {
    let numbered = sheet.rows.iter().any(|r| r.track.is_some());
    let mut by_track: BTreeMap<u16, &Row> = BTreeMap::new();
    for row in &sheet.rows {
        match row.track {
            Some(track) => {
                by_track.insert(track, row);
            }
            None if !numbered => {
                by_track.insert(row.channel, row);
            }
            None => {}
        }
    }
    (by_track, !numbered)
}

/// The DAW's tracks in order. A gap in the middle becomes a real, empty track,
/// otherwise every track after it records the wrong channel.
fn track_list(sheet: &Sheet, report: &mut Report) -> Vec<Track> {
    let (by_track, from_channels) = track_map(sheet);
    if from_channels && !by_track.is_empty() {
        report
            .warnings
            .push("the sheet has no Track column, so tracks follow the channel numbers".into());
    }
    let last = by_track.keys().copied().max().unwrap_or(0);
    let mut tracks = Vec::new();
    for n in 1..=last {
        match by_track.get(&n) {
            Some(row) => tracks.push(Track {
                output: n,
                name: row.daw_name(),
                channel: Some(row.channel),
                colour: row.colour,
            }),
            None => {
                report.filler += 1;
                tracks.push(Track {
                    output: n,
                    name: format!("Track {n}"),
                    channel: None,
                    colour: None,
                });
            }
        }
    }
    if report.filler > 0 {
        let n = report.filler;
        report.warnings.push(if n == 1 {
            "one track in the middle of the range carries no channel; it is created empty so \
             the tracks after it stay on the right inputs"
                .to_string()
        } else {
            format!(
                "{n} tracks in the middle of the range carry no channel; they are created \
                 empty so the tracks after them stay on the right inputs"
            )
        });
    }
    tracks
}

struct Writer<'a> {
    root: &'a mut Value,
    report: Report,
}

impl Writer<'_> {
    // ------------------------------------------------------------ channels ---

    fn channel(&mut self, row: &Row, req: &BuildRequest) {
        let n = row.channel;
        let ch = format!("ae_data/ch/{n}");
        if !self.exists(&ch) {
            self.warn(format!("line {}: the base snapshot has no channel {n}", row.line));
            return;
        }
        self.report.channels.push(n);

        if let Some(name) = &row.name {
            self.set_str(&format!("{ch}/name"), name);
        }
        if let Some(c) = row.colour {
            self.set_num(&format!("{ch}/col"), c as f64);
        }
        if let Some(i) = row.icon {
            self.set_num(&format!("{ch}/icon"), i as f64);
        }
        if let Some(m) = row.mute {
            self.set_bool(&format!("{ch}/mute"), m);
        }
        if let Some(f) = row.fader {
            self.set_num(&format!("{ch}/fdr"), f);
        }
        if let Some(p) = row.pan {
            self.set_num(&format!("{ch}/pan"), p);
        }
        if let Some(l) = row.link {
            self.set_bool(&format!("{ch}/clink"), l);
        }
        if let Some(m) = row.main {
            self.set_bool(&format!("{ch}/main/1/on"), m);
        }
        if let Some(cut) = row.low_cut {
            self.set_bool(&format!("{ch}/flt/lc"), cut.is_some());
            if let Some(hz) = cut {
                self.set_num(&format!("{ch}/flt/lcf"), hz);
            }
        }
        if let Some(sends) = &row.sends {
            self.sends(&ch, row, sends);
        }
        if row.dca.is_some() || row.mute_group.is_some() {
            self.tags(&ch, row);
        }
        if let Some(src) = &row.source {
            self.set_str(&format!("{ch}/in/conn/grp"), &src.group);
            if !src.is_off() {
                self.set_num(&format!("{ch}/in/conn/in"), src.index as f64);
            }
        }
        self.preamp(row, req);
    }

    /// Gain, phantom and polarity, written onto the socket the channel takes.
    fn preamp(&mut self, row: &Row, req: &BuildRequest) {
        if row.gain.is_none()
            && row.phantom.is_none()
            && row.polarity.is_none()
            && !(req.label_sources && (row.name.is_some() || row.colour.is_some() || row.icon.is_some()))
        {
            return;
        }
        let ch = format!("ae_data/ch/{}", row.channel);
        // The sheet's own Source wins; otherwise follow what the base has, so a
        // sheet that only sets gain still finds the right preamp.
        let src = row.source.clone().or_else(|| self.patched_source(&ch));
        let Some(src) = src.filter(|s| !s.is_off()) else {
            if row.gain.is_some() || row.phantom.is_some() {
                self.warn(format!(
                    "line {}: channel {} has no input patched, so its gain and phantom \
                     had nowhere to go",
                    row.line, row.channel
                ));
            }
            if let Some(p) = row.polarity {
                self.set_bool(&format!("{ch}/in/set/inv"), p);
            }
            return;
        };
        let socket = format!("ae_data/io/in/{}/{}", src.group, src.index);
        if !self.exists(&socket) {
            self.warn(format!(
                "line {}: the base snapshot has no input {src} for channel {}",
                row.line, row.channel
            ));
            return;
        }
        if let Some(g) = row.gain {
            self.set_or_warn(&format!("{socket}/g"), g, row, "gain", src.clone());
        }
        if let Some(p) = row.phantom {
            if self.exists(&format!("{socket}/vph")) {
                self.set_bool(&format!("{socket}/vph"), p);
            } else if p {
                self.warn(format!(
                    "line {}: {src} has no phantom power to switch on",
                    row.line
                ));
            }
        }
        if let Some(p) = row.polarity {
            // Digital sources have no preamp polarity, but the channel does.
            if self.exists(&format!("{socket}/pol")) {
                self.set_bool(&format!("{socket}/pol"), p);
            } else {
                self.set_bool(&format!("{ch}/in/set/inv"), p);
            }
        }
        // The desk can be set to show the source's own customisation on the
        // scribble strip rather than the channel's, so the two are kept
        // together: whichever the console is showing, it shows the patch sheet.
        if req.label_sources {
            if let Some(name) = &row.name {
                if self.exists(&format!("{socket}/name")) {
                    self.set_str(&format!("{socket}/name"), name);
                }
            }
            if let Some(c) = row.colour {
                if self.exists(&format!("{socket}/col")) {
                    self.set_num(&format!("{socket}/col"), c as f64);
                }
            }
            if let Some(i) = row.icon {
                if self.exists(&format!("{socket}/icon")) {
                    self.set_num(&format!("{socket}/icon"), i as f64);
                }
            }
        }
    }

    fn set_or_warn(&mut self, path: &str, v: f64, row: &Row, what: &str, src: SourceRef) {
        if self.exists(path) {
            self.set_num(path, v);
        } else {
            self.warn(format!("line {}: {src} has no {what} to set", row.line));
        }
    }

    fn sends(&mut self, ch: &str, row: &Row, sends: &[crate::sheet::Send]) {
        // A sheet that lists sends is stating the whole picture for that
        // channel, so buses it leaves out are switched off rather than left
        // wherever the base had them.
        let mut bus = 1u16;
        while self.exists(&format!("{ch}/send/{bus}")) {
            let want = sends.iter().find(|s| s.bus == bus);
            match want {
                Some(send) => {
                    self.set_bool(&format!("{ch}/send/{bus}/on"), send.level > -144.0);
                    self.set_num(&format!("{ch}/send/{bus}/lvl"), send.level);
                    if let Some(pre) = send.pre {
                        self.set_str(
                            &format!("{ch}/send/{bus}/mode"),
                            if pre { "PRE" } else { "POST" },
                        );
                    }
                }
                None if bus <= 16 => {
                    self.set_bool(&format!("{ch}/send/{bus}/on"), false);
                }
                None => {}
            }
            bus += 1;
        }
        let missing: Vec<u16> = sends
            .iter()
            .map(|s| s.bus)
            .filter(|b| !self.exists(&format!("{ch}/send/{b}")))
            .collect();
        for bus in missing {
            self.warn(format!(
                "line {}: the base snapshot has no bus {bus} on channel {}",
                row.line, row.channel
            ));
        }
    }

    /// DCA and mute-group membership live in the channel's tag string, as
    /// `#D1`..`#D16` and `#M1`..`#M8`. Tags the user put there for their own
    /// grouping are left where they are.
    fn tags(&mut self, ch: &str, row: &Row) {
        let path = format!("{ch}/tags");
        if !self.exists(&path) {
            self.warn(format!("line {}: the base snapshot has no tags on channel {}", row.line, row.channel));
            return;
        }
        let current = self.get(&path).and_then(Value::as_str).unwrap_or("").to_string();
        let mut kept: Vec<String> = current
            .split_whitespace()
            .filter(|t| !is_dca_tag(t) || row.dca.is_none())
            .filter(|t| !is_mute_tag(t) || row.mute_group.is_none())
            .map(str::to_string)
            .collect();
        for d in row.dca.iter().flatten() {
            kept.push(format!("#D{d}"));
        }
        for m in row.mute_group.iter().flatten() {
            kept.push(format!("#M{m}"));
        }
        let joined = kept.join(" ");
        if joined.chars().count() > 80 {
            self.warn(format!(
                "line {}: channel {}'s tags come to more than the console's 80 characters",
                row.line, row.channel
            ));
        }
        self.set_str(&path, &joined);
    }

    // ------------------------------------------------------ record outputs ---

    /// Point each listed output of the record group at its channel, so DAW
    /// track N really does carry the channel the sheet put on track N.
    fn record_patch(&mut self, sheet: &Sheet, group: &str, req: &BuildRequest) {
        let (by_track, _) = track_map(sheet);
        let outputs = self.count(&format!("ae_data/io/out/{group}"));
        for (track, row) in &by_track {
            let out = format!("ae_data/io/out/{group}/{track}");
            if !self.exists(&out) {
                self.warn(format!(
                    "line {}: {group} has {outputs} outputs, so track {track} is out of reach",
                    row.line
                ));
                continue;
            }
            self.set_str(&format!("{out}/grp"), "CH");
            self.set_num(&format!("{out}/in"), row.channel as f64);
        }
        if !req.keep_unlisted_outputs {
            for n in 1..=outputs as u16 {
                if by_track.contains_key(&n) {
                    continue;
                }
                let out = format!("ae_data/io/out/{group}/{n}");
                if self.get(&format!("{out}/grp")).and_then(Value::as_str) != Some("OFF") {
                    self.set_str(&format!("{out}/grp"), "OFF");
                    self.report.cleared += 1;
                }
            }
        }

        // The track list itself is the same on any desk, so it is worked out
        // in one place.
        self.report.tracks = track_list(sheet, &mut self.report);
    }

    /// Say when the file was written, and leave the rest of the base's identity
    /// alone - a snapshot built on someone's show file is still their file.
    fn stamp(&mut self) {
        if let Some(obj) = self.root.as_object_mut() {
            obj.insert("created".into(), Value::String(now()));
        }
    }

    // -------------------------------------------------------------- access ---

    fn get(&self, path: &str) -> Option<&Value> {
        let mut node = &*self.root;
        for key in path.split('/') {
            node = node.get(key)?;
        }
        Some(node)
    }

    fn exists(&self, path: &str) -> bool {
        self.get(path).is_some()
    }

    fn count(&self, path: &str) -> usize {
        self.get(path).and_then(Value::as_object).map(Map::len).unwrap_or(0)
    }

    fn patched_source(&self, ch: &str) -> Option<SourceRef> {
        let grp = self.get(&format!("{ch}/in/conn/grp"))?.as_str()?.to_string();
        let index = self.get(&format!("{ch}/in/conn/in"))?.as_i64().unwrap_or(0) as u16;
        Some(SourceRef { group: grp, index })
    }

    fn output_groups(&self) -> Vec<String> {
        self.get("ae_data/io/out")
            .and_then(Value::as_object)
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn warn(&mut self, text: String) {
        if !self.report.warnings.contains(&text) {
            self.report.warnings.push(text);
        }
    }

    fn set_str(&mut self, path: &str, v: &str) {
        self.put(path, Value::String(v.to_string()));
    }

    fn set_bool(&mut self, path: &str, v: bool) {
        self.put(path, Value::Bool(v));
    }

    /// Whole numbers are written as integers, which is what the console and
    /// WING-Edit both do - `-144`, not `-144.0`.
    fn set_num(&mut self, path: &str, v: f64) {
        let value = if v.fract() == 0.0 && v.abs() < 1e15 {
            Value::from(v as i64)
        } else {
            Value::from(v)
        };
        self.put(path, value);
    }

    /// Write a node that already exists. Nothing is created: the tree's shape
    /// is the console's, and a node this tool invented would not be understood.
    fn put(&mut self, path: &str, value: Value) {
        let keys: Vec<&str> = path.split('/').collect();
        let Some((last, parents)) = keys.split_last() else { return };
        let mut node = &mut *self.root;
        for key in parents {
            match node.get_mut(*key) {
                Some(next) => node = next,
                None => {
                    self.warn(format!("the base snapshot has no {}", pretty(path)));
                    return;
                }
            }
        }
        let Some(obj) = node.as_object_mut() else {
            self.warn(format!("{path} is not a node in the base snapshot"));
            return;
        };
        let Some(slot) = obj.get_mut(*last) else {
            self.warn(format!("the base snapshot has no {}", pretty(path)));
            return;
        };
        if *slot == value {
            return;
        }
        self.report.changes.push(Change {
            path: pretty(path),
            from: slot.clone(),
            to: value.clone(),
        });
        *slot = value;
    }
}

/// `ae_data/ch/1/name` reads better as the address the console uses.
fn pretty(path: &str) -> String {
    match path.strip_prefix("ae_data/") {
        Some(rest) => format!("/{rest}"),
        None => format!("/{path}"),
    }
}

fn is_dca_tag(t: &str) -> bool {
    t.strip_prefix("#D").map(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).unwrap_or(false)
}

fn is_mute_tag(t: &str) -> bool {
    t.strip_prefix("#M").map(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).unwrap_or(false)
}

/// `2026-09-10 12:34:56` in UTC, shaped the way the console writes the field.
/// The desk stamps local time; matching that would mean carrying a timezone
/// database for a string nothing reads back.
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Days-since-epoch to a calendar date (Howard Hinnant's civil_from_days).
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, (rem / 60 % 60) as u32, (rem % 60) as u32)
}

/// The console's colour for a strip, as the RGBA word Ardour stores on a
/// track - so a track in the DAW is the colour of the channel feeding it.
pub fn track_colour(idx: u8) -> Option<u32> {
    let rgb = crate::sheet::colour_rgb(idx)?;
    Some((rgb << 8) | 0xff)
}

/// Names the DAW can use: unique, and free of the characters a port name
/// cannot carry. Kept here so the snapshot and the session agree.
pub fn daw_names(tracks: &[Track]) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for track in tracks {
        let cleaned = track.name.trim().replace(['/', '\\', ':'], "-");
        let base = if cleaned.is_empty() { format!("Track {}", track.output) } else { cleaned };
        let mut candidate = base.clone();
        let mut n = 2;
        while !seen.insert(candidate.to_lowercase()) {
            candidate = format!("{base} {n}");
            n += 1;
        }
        out.push(candidate);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sheet;

    fn built(text: &str) -> Built {
        let sheet = sheet::parse(text).expect("sheet should parse");
        build(&sheet, &BuildRequest::default()).expect("build should succeed")
    }

    /// The snapshot a WING build produces. Desks without one are tested
    /// through their report instead.
    fn tree(out: &Built) -> &Value {
        out.snapshot.as_ref().expect("this desk writes a snapshot")
    }

    fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
        let mut node = v;
        for key in path.split('/') {
            node = node.get(key).unwrap_or_else(|| panic!("no node at {path}"));
        }
        node
    }

    /// A build for a desk that has no snapshot format.
    fn deskless(text: &str, desk: Desk) -> Built {
        let sheet = sheet::parse(text).expect("sheet should parse");
        build(&sheet, &BuildRequest { desk, ..BuildRequest::default() })
            .expect("build should succeed")
    }

    #[test]
    fn a_qu_builds_the_session_and_no_console_file() {
        let out = deskless("Ch,Name,Track\n1,Kick,1\n2,Snare,2\n3,Hat,3\n", Desk::Qu16);
        assert!(out.snapshot.is_none(), "a Qu has no file format for this to write");
        assert_eq!(daw_names(&out.report.tracks), ["Kick", "Snare", "Hat"]);
        // ...and asking for one says so rather than writing something useless.
        let path = std::env::temp_dir().join("wltb-should-not-appear.snap");
        assert!(out.write_snap(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn a_sheet_with_no_track_column_records_its_channels_in_order() {
        // A Qu-Drive records its inputs in order, so a sheet that never
        // mentions tracks still describes a session.
        let out = deskless("Ch,Name\n1,Kick\n2,Snare\n3,Hat\n", Desk::Qu16);
        assert_eq!(daw_names(&out.report.tracks), ["Kick", "Snare", "Hat"]);
        assert!(
            out.report.warnings.iter().any(|w| w.contains("no Track column")),
            "it should say it made that assumption: {:?}",
            out.report.warnings
        );
    }

    #[test]
    fn a_qu_says_which_columns_it_could_not_use() {
        // The sheet is the same one the WING build uses, so it will have
        // columns a Qu cannot take. Ignoring them quietly would leave the
        // sheet looking obeyed.
        let out = deskless("Ch,Name,Gain,48V,DCA\n1,Kick,32,Yes,1\n", Desk::Qu16);
        let warning = out
            .report
            .warnings
            .iter()
            .find(|w| w.contains("not applied"))
            .unwrap_or_else(|| panic!("expected a warning: {:?}", out.report.warnings));
        for column in ["Gain", "48V", "DCA"] {
            assert!(warning.contains(column), "{column} missing from {warning:?}");
        }
    }

    #[test]
    fn a_qu_16_notices_channels_it_does_not_have() {
        let small = deskless("Ch,Name\n1,Kick\n20,Spare\n", Desk::Qu16);
        assert!(
            small.report.warnings.iter().any(|w| w.contains("past the 16 inputs")),
            "{:?}",
            small.report.warnings
        );
        // The same sheet is fine on a desk that has the input.
        let large = deskless("Ch,Name\n1,Kick\n20,Spare\n", Desk::Qu32);
        assert!(!large.report.warnings.iter().any(|w| w.contains("past the")));
    }

    #[test]
    fn a_gap_in_the_tracks_stays_a_gap() {
        // Track 2 is empty, so track 3 is still the channel the sheet put
        // there rather than sliding up one.
        let out = deskless("Ch,Name,Track\n1,Kick,1\n3,Hat,3\n", Desk::Qu16);
        assert_eq!(daw_names(&out.report.tracks), ["Kick", "Track 2", "Hat"]);
        assert_eq!(out.report.filler, 1);
    }

    #[test]
    fn the_tree_keeps_its_shape() {
        // Nothing is created and nothing is removed: a WING snapshot is a fixed
        // tree, and a node this tool invented would not be understood.
        fn count(v: &Value) -> usize {
            match v.as_object() {
                Some(o) => o.values().map(count).sum(),
                None => 1,
            }
        }
        let base: Value = serde_json::from_str(FACTORY).unwrap();
        let out = built("Ch,Name,Source,Gain\n1,Kick,LCL 1,32\n");
        assert_eq!(count(&base), count(tree(&out)));
    }

    #[test]
    fn the_preamp_is_written_onto_the_socket_the_channel_takes() {
        // The sheet reads as though gain belonged to the channel; the console
        // keeps it on the input, so the Source column is what steers it.
        let out = built("Ch,Name,Source,Gain,48V,Pol\n5,Hat,LCL 9,26,Yes,Yes\n");
        assert_eq!(at(tree(&out), "ae_data/io/in/LCL/9/g"), 26);
        assert_eq!(at(tree(&out), "ae_data/io/in/LCL/9/vph"), true);
        assert_eq!(at(tree(&out), "ae_data/io/in/LCL/9/pol"), true);
        assert_eq!(at(tree(&out), "ae_data/ch/5/in/conn/grp"), "LCL");
        assert_eq!(at(tree(&out), "ae_data/ch/5/in/conn/in"), 9);
        // ...and the socket that channel 5 used to own is left alone.
        assert_eq!(at(tree(&out), "ae_data/io/in/LCL/5/g"), 0);
    }

    #[test]
    fn a_digital_source_has_no_gain_and_says_so() {
        // USB and StageConnect arrive at line level: there is no preamp to set,
        // and quietly doing nothing would leave the sheet looking obeyed.
        let out = built("Ch,Name,Source,Gain,48V,Pol\n1,Trax L,USB 1,12,Yes,Yes\n");
        assert!(
            out.report.warnings.iter().any(|w| w.contains("USB 1 has no gain")),
            "{:?}",
            out.report.warnings
        );
        // Polarity is not a preamp trick, so the digital input keeps that one.
        assert_eq!(at(tree(&out), "ae_data/io/in/USB/1/pol"), true);
    }

    #[test]
    fn a_channel_with_nothing_patched_keeps_its_polarity_on_the_channel() {
        let out = built("Ch,Name,Source,Gain,Pol\n1,Spare,Off,20,Yes\n");
        assert_eq!(at(tree(&out), "ae_data/ch/1/in/conn/grp"), "OFF");
        assert_eq!(at(tree(&out), "ae_data/ch/1/in/set/inv"), true);
        assert!(
            out.report.warnings.iter().any(|w| w.contains("nowhere to go")),
            "{:?}",
            out.report.warnings
        );
    }

    #[test]
    fn dca_and_mute_groups_are_written_as_the_consoles_tags() {
        let out = built("Ch,Name,DCA,MuteGrp\n1,Kick,\"1,2\",3\n");
        assert_eq!(at(tree(&out), "ae_data/ch/1/tags"), "#D1 #D2 #M3");
    }

    #[test]
    fn tags_of_your_own_survive_a_dca_change() {
        let mut base: Value = serde_json::from_str(FACTORY).unwrap();
        base["ae_data"]["ch"]["1"]["tags"] = Value::String("#D9 STAGE-LEFT".into());
        let dir = std::env::temp_dir().join(format!("wing-sheet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("base.snap");
        std::fs::write(&path, serde_json::to_string(&base).unwrap()).unwrap();

        let sheet = sheet::parse("Ch,Name,DCA\n1,Kick,3\n").unwrap();
        let out = build(
            &sheet,
            &BuildRequest { base: Some(path), ..BuildRequest::default() },
        )
        .unwrap();
        assert_eq!(at(tree(&out), "ae_data/ch/1/tags"), "STAGE-LEFT #D3");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_sends_states_the_whole_picture() {
        // A sheet that names bus 1 and 3 is saying bus 2 is off, not saying
        // nothing about bus 2.
        let out = built("Ch,Name,Sends\n1,Kick,\"1:-6,3:0\"\n");
        assert_eq!(at(tree(&out), "ae_data/ch/1/send/1/on"), true);
        assert_eq!(at(tree(&out), "ae_data/ch/1/send/1/lvl"), -6);
        assert_eq!(at(tree(&out), "ae_data/ch/1/send/2/on"), false);
        assert_eq!(at(tree(&out), "ae_data/ch/1/send/3/on"), true);
    }

    #[test]
    fn the_record_patch_puts_each_channel_on_its_own_track() {
        let out = built("Ch,Name,Track\n5,Bass,1\n9,Vox,2\n");
        assert_eq!(at(tree(&out), "ae_data/io/out/USB/1/grp"), "CH");
        assert_eq!(at(tree(&out), "ae_data/io/out/USB/1/in"), 5);
        assert_eq!(at(tree(&out), "ae_data/io/out/USB/2/in"), 9);
        assert_eq!(at(tree(&out), "ae_data/io/out/USB/3/grp"), "OFF");
        assert_eq!(out.report.tracks.len(), 2);
        assert_eq!(out.report.tracks[0].name, "Bass");
    }

    #[test]
    fn a_gap_in_the_track_column_becomes_a_real_empty_track() {
        // Track 3 has no channel. If it were simply left out, everything after
        // it would record one input early.
        let out = built("Ch,Name,Track\n1,Kick,1\n2,Snare,2\n3,Vox,4\n");
        assert_eq!(out.report.filler, 1);
        let names: Vec<&str> = out.report.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Kick", "Snare", "Track 3", "Vox"]);
        assert!(out.report.tracks[2].channel.is_none());
        assert_eq!(at(tree(&out), "ae_data/io/out/USB/4/in"), 3);
    }

    #[test]
    fn whole_numbers_are_written_as_integers() {
        // What the desk and WING-Edit both write: -144, not -144.0.
        let out = built("Ch,Name,Fader,LowCut\n1,Kick,-6,31.5\n");
        let text = serde_json::to_string(&out.snapshot).unwrap();
        assert!(text.contains("\"fdr\":-6,"), "fader should be a plain integer");
        assert!(text.contains("\"lcf\":31.5"), "a fractional value stays fractional");
    }

    #[test]
    fn a_track_beyond_the_groups_outputs_is_named_not_dropped() {
        let sheet = sheet::parse("Ch,Name,Track\n1,Kick,60\n").unwrap();
        let out = build(
            &sheet,
            &BuildRequest { record_group: "LCL".into(), ..BuildRequest::default() },
        )
        .unwrap();
        assert!(
            out.report.warnings.iter().any(|w| w.contains("out of reach")),
            "{:?}",
            out.report.warnings
        );
    }

    #[test]
    fn an_unknown_record_group_lists_the_ones_there_are() {
        let sheet = sheet::parse("Ch,Name,Track\n1,Kick,1\n").unwrap();
        let err = build(
            &sheet,
            &BuildRequest { record_group: "DANTE".into(), ..BuildRequest::default() },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("USB"), "{err}");
    }

    #[test]
    fn a_track_is_the_colour_of_the_channel_feeding_it() {
        // RED on the sheet has to reach LiveTrax as the console's red, not a
        // red of our own choosing.
        let red = crate::sheet::colour_rgb(9).unwrap();
        assert_eq!(red, 0xE01F41);
        assert_eq!(track_colour(9), Some(0xE01F41FF));
        for (idx, _, _) in crate::sheet::COLOURS {
            assert!(track_colour(idx).is_some(), "colour {idx} has no track colour");
        }
        assert_eq!(track_colour(0), None, "an index the console has no colour for");
    }

    #[test]
    fn duplicate_track_names_are_made_unique_for_the_daw() {
        let tracks = vec![
            Track { output: 1, name: "Amb".into(), channel: Some(1), colour: None },
            Track { output: 2, name: "Amb".into(), channel: Some(2), colour: None },
            Track { output: 3, name: "Vox/Lead".into(), channel: Some(3), colour: None },
        ];
        assert_eq!(daw_names(&tracks), ["Amb", "Amb 2", "Vox-Lead"]);
    }

    #[test]
    fn the_shipped_template_builds_a_console() {
        let out = built(&sheet::template());
        assert_eq!(out.report.channels.len(), 24);
        assert_eq!(at(tree(&out), "ae_data/ch/1/name"), "Kick In");
        assert_eq!(at(tree(&out), "ae_data/ch/16/name"), "Lead Vox");
        assert_eq!(at(tree(&out), "ae_data/io/in/LCL/4/pol"), true);
        assert_eq!(at(tree(&out), "ae_data/ch/19/main/1/on"), false);
        assert_eq!(out.report.tracks.len(), 24);
    }
}
