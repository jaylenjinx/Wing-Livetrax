//! The patch list, as a spreadsheet.
//!
//! A live patch is planned in a spreadsheet long before anyone touches a
//! console: one row per channel, with the name, the socket it arrives on, the
//! gain and phantom the source needs, a colour, a DCA, and which track it
//! records to. This module reads that sheet. What is done with it lives in
//! [`crate::patchbuild`].
//!
//! The file is CSV (or tab- or semicolon-separated - Excel writes all three
//! depending on where it was installed). Everything about the reader is built
//! for sheets that people actually send each other rather than a strict
//! interchange format:
//!
//! * columns are found by **name**, in any order, matched loosely enough that
//!   "48V", "48v" and "Phantom" are one column;
//! * columns it does not recognise are **kept out of the way, not rejected** -
//!   a real patch list has Mic, Stand and Notes columns and should not have to
//!   lose them;
//! * an **empty cell means "leave this alone"**, not "set it to zero", so a
//!   sheet that only fills in Ch and Name changes only names;
//! * every complaint carries the line and the column it came from.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

// ------------------------------------------------------------------ model ---

/// A source or destination as the console writes it: a port group and a
/// 1-based index, e.g. `("LCL", 1)`. `("OFF", 0)` is "nothing patched".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    pub group: String,
    pub index: u16,
}

impl SourceRef {
    pub fn is_off(&self) -> bool {
        self.group == "OFF"
    }
}

impl std::fmt::Display for SourceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_off() {
            write!(f, "off")
        } else {
            write!(f, "{} {}", self.group, self.index)
        }
    }
}

/// One bus send: which bus, how loud, and whether it is pre-fader.
#[derive(Debug, Clone, PartialEq)]
pub struct Send {
    pub bus: u16,
    pub level: f64,
    pub pre: Option<bool>,
}

/// One row of the sheet: one console channel.
///
/// Every field but `channel` is optional, and `None` means the cell was empty.
/// The builder leaves those nodes as the base snapshot had them.
#[derive(Debug, Clone, Default)]
pub struct Row {
    /// Line in the file this row came from, for error messages.
    pub line: usize,
    pub channel: u16,
    pub name: Option<String>,
    pub source: Option<SourceRef>,
    pub gain: Option<f64>,
    pub phantom: Option<bool>,
    pub polarity: Option<bool>,
    /// `Some(None)` is "low cut off"; `Some(Some(hz))` is "on, at this cut".
    pub low_cut: Option<Option<f64>>,
    pub colour: Option<u8>,
    pub icon: Option<u16>,
    pub dca: Option<Vec<u8>>,
    pub mute_group: Option<Vec<u8>>,
    pub fader: Option<f64>,
    pub pan: Option<f64>,
    pub main: Option<bool>,
    pub mute: Option<bool>,
    pub sends: Option<Vec<Send>>,
    /// Link this channel to the next one as a stereo pair.
    pub link: Option<bool>,
    /// Output of the record group, and so the DAW track, that carries this
    /// channel. `None` means this channel is not recorded.
    pub track: Option<u16>,
    /// Track name, when it should differ from the channel name.
    pub track_name: Option<String>,
    /// Cells from columns the reader does not know about, kept so the report
    /// can show them and nothing is silently lost.
    pub extra: BTreeMap<String, String>,
}

impl Row {
    /// The name this channel's track should carry.
    pub fn daw_name(&self) -> String {
        self.track_name
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| format!("Ch {}", self.channel))
    }
}

/// A whole sheet, read and checked.
#[derive(Debug, Clone, Default)]
pub struct Sheet {
    pub rows: Vec<Row>,
    /// Headers that matched no known column, in the order they appeared.
    pub unknown_columns: Vec<String>,
    /// Things worth saying that are not bad enough to refuse the file.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------- reading ---

/// Read a patch sheet from disk.
pub fn read(path: &Path) -> Result<Sheet> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = decode(&bytes)
        .with_context(|| format!("{} is not text - export it as CSV", path.display()))?;
    parse(&text).with_context(|| format!("reading {}", path.display()))
}

/// Spreadsheets are saved as UTF-8 more often than not, but Excel on Windows
/// still writes Latin-1, and both like a byte-order mark.
fn decode(bytes: &[u8]) -> Result<String> {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        bail!("this looks like a UTF-16 file; save it as CSV UTF-8");
    }
    if body.starts_with(b"PK\x03\x04") {
        bail!("this is a .xlsx workbook; use File > Save As and choose CSV");
    }
    match std::str::from_utf8(body) {
        Ok(s) => Ok(s.to_string()),
        // Latin-1 never fails, and for a patch list the difference is only ever
        // in a name, so decoding it is better than refusing the file.
        Err(_) => Ok(body.iter().map(|&b| b as char).collect()),
    }
}

pub fn parse(text: &str) -> Result<Sheet> {
    let table = split_table(text);
    let filled: Vec<&(usize, Vec<String>)> = table
        .iter()
        .filter(|(_, cells)| cells.iter().any(|c| !c.trim().is_empty()))
        .collect();
    let first = *filled.first().ok_or_else(|| anyhow!("the sheet is empty"))?;
    // A patch sheet usually opens with the name of the show, sometimes with a
    // date under it, so the header is the first row that names a channel
    // column rather than simply the first row with anything in it.
    let (header_line, headers) = filled
        .iter()
        .take(10)
        .find(|(_, cells)| cells.iter().any(|c| Col::of(c.trim()) == Some(Col::Channel)))
        .map(|(line, cells)| (*line, cells.clone()))
        .unwrap_or_else(|| (first.0, first.1.clone()));

    let mut sheet = Sheet::default();
    let mut map: BTreeMap<Col, usize> = BTreeMap::new();
    for (i, head) in headers.iter().enumerate() {
        let head = head.trim();
        if head.is_empty() {
            continue;
        }
        match Col::of(head) {
            Some(col) => {
                if let Some(first) = map.insert(col, i) {
                    sheet.warnings.push(format!(
                        "line {header_line}: {:?} and {:?} both mean {}; using the first",
                        headers[first].trim(),
                        head,
                        col.canonical()
                    ));
                    map.insert(col, first);
                }
            }
            None => sheet.unknown_columns.push(head.to_string()),
        }
    }

    map.get(&Col::Channel).ok_or_else(|| {
        anyhow!(
            "no channel column: the sheet needs a column headed \"Ch\" (line {header_line} \
             reads {})",
            headers
                .iter()
                .map(|h| h.trim())
                .filter(|h| !h.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if !map.contains_key(&Col::Name) {
        sheet
            .warnings
            .push("no Name column, so channel names are left as the base snapshot had them".into());
    }

    let mut seen: BTreeMap<u16, usize> = BTreeMap::new();
    for (line, cells) in table.iter().filter(|(l, _)| *l > header_line) {
        let cell = |col: Col| -> &str {
            map.get(&col)
                .and_then(|i| cells.get(*i))
                .map(|s| s.trim())
                .unwrap_or("")
        };
        if cells.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let raw_ch = cell(Col::Channel);
        if raw_ch.is_empty() {
            // A sheet often carries a blank spacer row, or a section heading in
            // the name column. Neither is a channel, and neither is an error.
            continue;
        }
        let at = |col: Col, e: anyhow::Error| anyhow!("line {line}, {}: {e}", col.canonical());

        let mut row = Row {
            line: *line,
            channel: number(raw_ch)
                .and_then(|n| {
                    (1.0..=40.0)
                        .contains(&n)
                        .then_some(n as u16)
                        .ok_or_else(|| anyhow!("{raw_ch:?} is not a channel between 1 and 40"))
                })
                .map_err(|e| at(Col::Channel, e))?,
            ..Default::default()
        };
        if let Some(first) = seen.insert(row.channel, *line) {
            sheet.warnings.push(format!(
                "line {line}: channel {} was already set on line {first}; the later row wins",
                row.channel
            ));
        }

        for (col, i) in &map {
            let value = cells.get(*i).map(|s| s.trim()).unwrap_or("");
            if value.is_empty() {
                continue;
            }
            col.apply(&mut row, value).map_err(|e| at(*col, e))?;
        }
        for head in &sheet.unknown_columns {
            let Some(i) = headers.iter().position(|h| h.trim() == head) else { continue };
            let value = cells.get(i).map(|s| s.trim()).unwrap_or("");
            if !value.is_empty() {
                row.extra.insert(head.clone(), value.to_string());
            }
        }
        sheet.rows.push(row);
    }

    if sheet.rows.is_empty() {
        bail!("no channel rows under the header on line {header_line}");
    }
    sheet.rows.sort_by_key(|r| r.channel);
    check(&mut sheet);
    Ok(sheet)
}

/// Complaints that only make sense once the whole sheet is in view.
fn check(sheet: &mut Sheet) {
    let mut by_source: BTreeMap<String, Vec<u16>> = BTreeMap::new();
    let mut by_track: BTreeMap<u16, Vec<u16>> = BTreeMap::new();
    for row in &sheet.rows {
        if let Some(src) = &row.source {
            if !src.is_off() {
                by_source.entry(src.to_string()).or_default().push(row.channel);
            }
        }
        if let Some(track) = row.track {
            by_track.entry(track).or_default().push(row.channel);
        }
    }
    for (src, channels) in by_source.iter().filter(|(_, c)| c.len() > 1) {
        sheet.warnings.push(format!(
            "{src} feeds channels {} - the console allows it, but check it is deliberate",
            list(channels)
        ));
    }
    for (track, channels) in by_track.iter().filter(|(_, c)| c.len() > 1) {
        sheet.warnings.push(format!(
            "channels {} all record to track {track}; only the last one will get there",
            list(channels)
        ));
    }
    for row in &sheet.rows {
        if let Some(name) = &row.name {
            if name.chars().count() > 16 {
                sheet.warnings.push(format!(
                    "line {}: {name:?} is longer than the console's 16 characters",
                    row.line
                ));
            }
        }
    }
}

fn list(v: &[u16]) -> String {
    v.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
}

// --------------------------------------------------------------- CSV bones ---

/// Split the file into (line number, cells), handling quotes, both line
/// endings, and `#` comments.
fn split_table(text: &str) -> Vec<(usize, Vec<String>)> {
    let delim = sniff(text);
    let mut out = Vec::new();
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut line = 1usize;
    let mut start = 1usize;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                if c == '\n' {
                    line += 1;
                }
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.trim().is_empty() => {
                cell.clear();
                quoted = true;
            }
            c if c == delim => {
                cells.push(std::mem::take(&mut cell));
            }
            '\r' => {}
            '\n' => {
                cells.push(std::mem::take(&mut cell));
                push_row(&mut out, start, std::mem::take(&mut cells));
                line += 1;
                start = line;
            }
            _ => cell.push(c),
        }
    }
    if !cell.is_empty() || !cells.is_empty() {
        cells.push(cell);
        push_row(&mut out, start, cells);
    }
    out
}

fn push_row(out: &mut Vec<(usize, Vec<String>)>, line: usize, cells: Vec<String>) {
    // A comment line is one whose first cell starts with '#'. A cell containing
    // a '#' later on is a note, not a comment.
    if cells.first().map(|c| c.trim_start().starts_with('#')).unwrap_or(false) {
        return;
    }
    out.push((line, cells));
}

/// Excel picks its separator from the machine's locale, so guess from the
/// header: the candidate that splits the first line into the most columns.
fn sniff(text: &str) -> char {
    let head = text
        .lines()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .unwrap_or("");
    [',', '\t', ';']
        .into_iter()
        .max_by_key(|d| head.matches(*d).count())
        .filter(|d| head.contains(*d))
        .unwrap_or(',')
}

// ----------------------------------------------------------------- columns ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Col {
    Channel,
    Name,
    Source,
    Gain,
    Phantom,
    Polarity,
    LowCut,
    Colour,
    Icon,
    Dca,
    MuteGroup,
    Fader,
    Pan,
    Main,
    Mute,
    Sends,
    Link,
    Track,
    TrackName,
}

impl Col {
    pub fn canonical(self) -> &'static str {
        match self {
            Col::Channel => "Ch",
            Col::Name => "Name",
            Col::Source => "Source",
            Col::Gain => "Gain",
            Col::Phantom => "48V",
            Col::Polarity => "Pol",
            Col::LowCut => "LowCut",
            Col::Colour => "Colour",
            Col::Icon => "Icon",
            Col::Dca => "DCA",
            Col::MuteGroup => "MuteGrp",
            Col::Fader => "Fader",
            Col::Pan => "Pan",
            Col::Main => "Main",
            Col::Mute => "Mute",
            Col::Sends => "Sends",
            Col::Link => "Link",
            Col::Track => "Track",
            Col::TrackName => "TrackName",
        }
    }

    /// Every spelling of a header that means this column.
    fn aliases(self) -> &'static [&'static str] {
        match self {
            Col::Channel => &["ch", "chan", "channel", "chno", "chnum", "no", "num", "#"],
            Col::Name => &["name", "channelname", "chname", "label", "scribble", "source name"],
            Col::Source => &["source", "src", "patch", "input", "in", "socket", "inputpatch", "jack"],
            Col::Gain => &["gain", "ha", "gaindb", "hagain", "preamp", "preampgain"],
            Col::Phantom => &["48v", "48", "phantom", "p48", "ph", "phantompower"],
            Col::Polarity => &["pol", "polarity", "phase", "invert", "inv"],
            Col::LowCut => &["lowcut", "lc", "hpf", "highpass", "hipass", "lowcuthz", "hp"],
            Col::Colour => &["colour", "color", "col"],
            Col::Icon => &["icon", "iconno"],
            Col::Dca => &["dca", "dcas", "vca", "dcagroup"],
            Col::MuteGroup => &["mutegrp", "mutegroup", "mgrp", "mutegroups", "mg"],
            Col::Fader => &["fader", "fdr", "level", "lvl", "faderdb"],
            Col::Pan => &["pan", "panning"],
            Col::Main => &["main", "mainassign", "lr", "tomain", "mainlr"],
            Col::Mute => &["mute", "muted"],
            Col::Sends => &["sends", "send", "bus", "buses", "busses", "bussends", "auxsends"],
            Col::Link => &["link", "stereo", "pair", "clink", "stereolink"],
            Col::Track => &["track", "trackno", "tracknumber", "rec", "record", "recordtrack", "out", "output", "tape"],
            Col::TrackName => &["trackname", "dawname", "tapename", "livetraxname", "daw"],
        }
    }

    fn of(header: &str) -> Option<Col> {
        let want = fold(header);
        if want.is_empty() {
            return None;
        }
        [
            Col::Channel,
            Col::Name,
            Col::Source,
            Col::Gain,
            Col::Phantom,
            Col::Polarity,
            Col::LowCut,
            Col::Colour,
            Col::Icon,
            Col::Dca,
            Col::MuteGroup,
            Col::Fader,
            Col::Pan,
            Col::Main,
            Col::Mute,
            Col::Sends,
            Col::Link,
            Col::Track,
            Col::TrackName,
        ]
        .into_iter()
        .find(|col| col.aliases().iter().any(|a| fold(a) == want))
    }

    fn apply(self, row: &mut Row, value: &str) -> Result<()> {
        match self {
            Col::Channel => {}
            Col::Name => row.name = Some(value.to_string()),
            Col::TrackName => row.track_name = Some(value.to_string()),
            Col::Source => row.source = Some(source(value)?),
            Col::Gain => row.gain = Some(ranged(value, -3.0, 45.5)?),
            Col::Phantom => row.phantom = Some(flag(value)?),
            Col::Polarity => row.polarity = Some(flag(value)?),
            Col::Mute => row.mute = Some(flag(value)?),
            Col::Main => row.main = Some(flag(value)?),
            Col::Link => row.link = Some(flag(value)?),
            Col::LowCut => row.low_cut = Some(low_cut(value)?),
            Col::Colour => row.colour = Some(colour(value)?),
            Col::Icon => row.icon = Some(ranged(value, 0.0, 999.0)? as u16),
            Col::Dca => row.dca = Some(indices(value, 16, "DCA")?),
            Col::MuteGroup => row.mute_group = Some(indices(value, 8, "mute group")?),
            Col::Fader => row.fader = Some(level(value)?),
            Col::Pan => row.pan = Some(ranged(value, -100.0, 100.0)?),
            Col::Sends => row.sends = Some(sends(value)?),
            Col::Track => row.track = Some(ranged(value, 1.0, 64.0)? as u16),
        }
        Ok(())
    }
}

/// Compare headers ignoring case, spaces and punctuation, so "Low Cut",
/// "low-cut" and "LOWCUT" are one column.
fn fold(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || *c == '#')
        .flat_map(char::to_lowercase)
        .collect()
}

// ------------------------------------------------------------------ values ---

/// A number as a spreadsheet writes it: units, a plus sign, a typographic
/// minus from a word processor, or thousands separators.
fn number(s: &str) -> Result<f64> {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '\u{2212}' | '\u{2013}' | '\u{2014}' => '-',
            c => c,
        })
        .filter(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.'))
        .collect();
    cleaned
        .parse::<f64>()
        .map_err(|_| anyhow!("{s:?} is not a number"))
}

fn ranged(s: &str, lo: f64, hi: f64) -> Result<f64> {
    let n = number(s)?;
    if n < lo || n > hi {
        bail!("{s:?} is outside {lo} to {hi}");
    }
    Ok(n)
}

/// A fader or send level: a number of dB, or one of the ways a sheet writes
/// silence.
fn level(s: &str) -> Result<f64> {
    let t = s.trim();
    if matches!(fold(t).as_str(), "off" | "inf" | "oo" | "" | "mute" | "closed") || t == "-∞" {
        return Ok(-144.0);
    }
    let n = number(t)?;
    if !(-144.0..=10.0).contains(&n) {
        bail!("{s:?} is outside -144 to +10 dB");
    }
    Ok(n)
}

/// A yes/no cell. Spreadsheets mark a box with anything from "Y" to a tick.
fn flag(s: &str) -> Result<bool> {
    // Ticks and boxes have no letters to fold, so they are matched as they were
    // typed and before anything else.
    match s.trim() {
        "\u{2713}" | "\u{2714}" | "\u{2611}" | "\u{25cf}" | "\u{2022}" => return Ok(true),
        "\u{2610}" | "\u{25cb}" => return Ok(false),
        _ => {}
    }
    match fold(s).as_str() {
        "y" | "yes" | "t" | "true" | "1" | "on" | "x" | "in" | "yep" => Ok(true),
        "n" | "no" | "f" | "false" | "0" | "off" | "out" | "" => Ok(false),
        _ => bail!("{:?} is not yes or no", s.trim()),
    }
}

/// `""`/`off` is the filter switched out; anything else is a cut frequency.
fn low_cut(s: &str) -> Result<Option<f64>> {
    if matches!(fold(s).as_str(), "off" | "no" | "n" | "out" | "0" | "-") {
        return Ok(None);
    }
    let hz = number(s)?;
    if !(20.0..=2000.0).contains(&hz) {
        bail!("{s:?} is outside the console's 20 to 2000 Hz low cut");
    }
    Ok(Some(hz))
}

/// The console's eighteen colours, read off a WING-Edit strip one index at a
/// time. They are a hue wheel rather than a short primary set, so a name table
/// is the only way a sheet can say "red" and mean the console's red.
///
/// The first name of each index is the one the template prints; the rest are
/// spellings people reach for. The RGB is what the desk draws, and is reused
/// for the DAW track so a strip and its track are the same colour.
pub const COLOURS: [(u8, u32, &[&str]); 18] = [
    (1, 0x3E63CC, &["blue", "bl"]),
    (2, 0x0080FF, &["sky", "azure"]),
    (3, 0x5A33FF, &["indigo", "violet"]),
    (4, 0x00CED1, &["teal", "turquoise"]),
    (5, 0x06B23E, &["green", "gn"]),
    (6, 0x96CB00, &["lime"]),
    (7, 0xF1DD00, &["yellow", "ye"]),
    (8, 0xBF6A1F, &["brown", "tan"]),
    (9, 0xE01F41, &["red", "rd"]),
    (10, 0xFF797A, &["salmon", "rose"]),
    (11, 0xFF32F6, &["magenta", "mg", "pink"]),
    (12, 0xA534FF, &["purple"]),
    (13, 0xFFB81A, &["orange", "amber", "or"]),
    (14, 0x25C3FF, &["cyan", "cy"]),
    (15, 0xFF5A30, &["coral", "tomato"]),
    (16, 0x33E6A5, &["mint"]),
    (17, 0x707070, &["grey", "gray", "gy"]),
    (18, 0xE0E0E0, &["white", "wh"]),
];

/// What the console draws for a colour index, as 0xRRGGBB.
pub fn colour_rgb(idx: u8) -> Option<u32> {
    COLOURS.iter().find(|(i, _, _)| *i == idx).map(|(_, rgb, _)| *rgb)
}

fn colour(s: &str) -> Result<u8> {
    let want = fold(s);
    if let Some((idx, _, _)) = COLOURS.iter().find(|(_, _, names)| names.contains(&want.as_str())) {
        return Ok(*idx);
    }
    let n = ranged(s, 1.0, 18.0).map_err(|_| {
        anyhow!(
            "{s:?} is not a colour: use 1-18, or one of {}",
            COLOURS
                .iter()
                .map(|(_, _, names)| names[0].to_uppercase())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    Ok(n as u8)
}

/// `1`, `1,2`, `1 2`, `DCA 1 / DCA 3` - all the ways a list of small numbers
/// gets written into one cell.
fn indices(s: &str, max: u8, what: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for part in s
        .split([',', ';', '/', '+', '&', ' ', '\n'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let digits: String = part.chars().filter(char::is_ascii_digit).collect();
        if digits.is_empty() {
            // "DCA" on its own before the number, e.g. "DCA 1"
            if part.chars().all(|c| c.is_alphabetic()) {
                continue;
            }
            bail!("{part:?} is not a {what} number");
        }
        let n: u8 = digits.parse().map_err(|_| anyhow!("{part:?} is not a {what} number"))?;
        if n < 1 || n > max {
            bail!("{what} {n} is outside 1 to {max}");
        }
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// A source reference: `LCL 1`, `LCL1`, `A 12`, `off`.
fn source(s: &str) -> Result<SourceRef> {
    let t = s.trim();
    if matches!(fold(t).as_str(), "off" | "none" | "no" | "-" | "x") || t == "–" {
        return Ok(SourceRef { group: "OFF".into(), index: 0 });
    }
    let split = t
        .char_indices()
        .find(|(_, c)| c.is_ascii_digit())
        .map(|(i, _)| i)
        .ok_or_else(|| anyhow!("{s:?} has no input number - write it like \"LCL 1\""))?;
    let (word, digits) = t.split_at(split);
    let group = group_name(word).ok_or_else(|| {
        anyhow!(
            "{:?} is not a port group - use one of {}",
            word.trim(),
            GROUPS.iter().map(|(_, g)| *g).collect::<Vec<_>>().join(", ")
        )
    })?;
    let index = number(digits)? as i64;
    if !(1..=64).contains(&index) {
        bail!("input {index} is outside 1 to 64");
    }
    Ok(SourceRef { group: group.to_string(), index: index as u16 })
}

/// Console port groups, and the spellings people use for them.
pub const GROUPS: [(&str, &str); 19] = [
    ("lcl", "LCL"),
    ("local", "LCL"),
    ("l", "LCL"),
    ("aux", "AUX"),
    ("a", "A"),
    ("aes50a", "A"),
    ("b", "B"),
    ("aes50b", "B"),
    ("c", "C"),
    ("aes50c", "C"),
    ("sc", "SC"),
    ("stageconnect", "SC"),
    ("usb", "USB"),
    ("crd", "CRD"),
    ("card", "CRD"),
    // A Qu's sockets, so a Qu sheet's Source column reads. Nothing is patched
    // from them - they are there to be printed and to survive the round trip.
    ("dsnake", "DSNAKE"),
    ("ds", "DSNAKE"),
    ("slink", "SLINK"),
    ("sl", "SLINK"),
];

fn group_name(word: &str) -> Option<&'static str> {
    let want = fold(word);
    GROUPS
        .iter()
        .find(|(alias, _)| *alias == want)
        .map(|(_, g)| *g)
        .or_else(|| {
            // Groups that need no alias table because their name is their code.
            ["MOD", "PLAY", "AES", "USR", "OSC", "BUS", "MAIN", "MTX", "SEND", "MON"]
                .into_iter()
                .find(|g| fold(g) == want)
        })
}

/// `1:-6, 3:0` - or `1@-6`, `1=-6`, or a bare `1,3` meaning "on, at unity".
fn sends(s: &str) -> Result<Vec<Send>> {
    let mut out: Vec<Send> = Vec::new();
    for part in s.split([',', ';', '\n']).map(str::trim).filter(|p| !p.is_empty()) {
        let (bus_txt, rest) = match part.split_once([':', '@', '=']) {
            Some((b, l)) => (b, Some(l)),
            None => (part, None),
        };
        let digits: String = bus_txt.chars().filter(char::is_ascii_digit).collect();
        let bus: u16 = digits
            .parse()
            .map_err(|_| anyhow!("{bus_txt:?} is not a bus number"))?;
        if !(1..=16).contains(&bus) {
            bail!("bus {bus} is outside 1 to 16");
        }
        // A trailing "p" marks the send pre-fader: "3:-6p".
        let (level_txt, pre) = match rest {
            Some(r) => {
                let r = r.trim();
                match r.strip_suffix(['p', 'P']) {
                    Some(head) => (head, Some(true)),
                    None => match r.strip_suffix(['o', 'O']) {
                        Some(head) => (head, Some(false)),
                        None => (r, None),
                    },
                }
            }
            None => ("0", None),
        };
        let level = level(level_txt)?;
        if out.iter().any(|x| x.bus == bus) {
            bail!("bus {bus} appears twice");
        }
        out.push(Send { bus, level, pre });
    }
    out.sort_by_key(|s| s.bus);
    Ok(out)
}

// ---------------------------------------------------------------- template ---

/// The starter sheet, with the column reference above it as comments and a
/// worked example below - a 24-piece band patch that actually adds up.
pub fn template() -> String {
    let header = "Ch,Name,Source,Gain,48V,Pol,LowCut,Colour,Icon,DCA,MuteGrp,Fader,Pan,Main,Sends,Link,Track,TrackName,Mic,Stand,Notes";
    let rows: &[&str] = &[
        "1,Kick In,LCL 1,32,No,No,30,RD,1,1,1,0,0,Yes,1:-6,No,1,Kick In,Beta91,Short,inside",
        "2,Kick Out,LCL 2,28,No,No,40,RD,1,1,1,0,0,Yes,1:-6,No,2,Kick Out,Beta52,Short,",
        "3,Snare Top,LCL 3,30,No,No,80,RD,2,1,1,0,0,Yes,\"1:-3,2:-10\",No,3,Snare Top,SM57,Short,",
        "4,Snare Bot,LCL 4,34,No,Yes,120,RD,2,1,1,0,0,Yes,1:-6,No,4,Snare Bot,SM57,Short,polarity flipped",
        "5,Hi Hat,LCL 5,26,Yes,No,150,RD,3,1,1,0,-40,Yes,,No,5,Hi Hat,KM184,Short,",
        "6,Rack Tom,LCL 6,24,No,No,80,RD,4,1,1,0,-30,Yes,1:-6,No,6,Rack Tom,e904,Clip,",
        "7,Floor Tom,LCL 7,22,No,No,60,RD,4,1,1,0,30,Yes,1:-6,No,7,Floor Tom,e904,Clip,",
        "8,OH L,LCL 8,36,Yes,No,150,RD,5,1,1,0,-70,Yes,,Yes,8,OH L,KM184,Tall,stereo pair",
        "9,OH R,LCL 9,36,Yes,No,150,RD,5,1,1,0,70,Yes,,No,9,OH R,KM184,Tall,",
        "10,Bass DI,LCL 10,12,Yes,No,Off,YE,6,2,1,0,0,Yes,1:-10,No,10,Bass DI,Radial,,",
        "11,Bass Amp,LCL 11,20,No,No,50,YE,6,2,1,0,0,Yes,1:-10,No,11,Bass Amp,MD421,Short,",
        "12,Gtr L,LCL 12,26,No,No,80,GN,7,3,1,0,-40,Yes,\"1:-6,3:-12\",Yes,12,Gtr L,SM57,Short,",
        "13,Gtr R,LCL 13,26,No,No,80,GN,7,3,1,0,40,Yes,\"1:-6,3:-12\",No,13,Gtr R,SM57,Short,",
        "14,Keys L,LCL 14,10,No,No,Off,GN,8,3,1,0,-60,Yes,1:-8,Yes,14,Keys L,DI,,",
        "15,Keys R,LCL 15,10,No,No,Off,GN,8,3,1,0,60,Yes,1:-8,No,15,Keys R,DI,,",
        "16,Lead Vox,LCL 16,38,No,No,100,BL,9,4,2,0,0,Yes,\"1:0,2:-6,3:-6\",No,16,Lead Vox,KSM9,Tall,",
        "17,BV 1,LCL 17,40,No,No,120,BL,9,4,2,-3,-20,Yes,\"1:-6,2:-10\",No,17,BV 1,SM58,Tall,",
        "18,BV 2,LCL 18,40,No,No,120,BL,9,4,2,-3,20,Yes,\"1:-6,2:-10\",No,18,BV 2,SM58,Tall,",
        "19,Talkback,LCL 19,30,Yes,No,150,WH,10,,,Off,0,No,,No,,,SM58,Desk,not recorded",
        "20,Playback L,USB 1,0,No,No,Off,MG,11,5,,0,-100,Yes,,Yes,20,Playback L,,,from the DAW",
        "21,Playback R,USB 2,0,No,No,Off,MG,11,5,,0,100,Yes,,No,21,Playback R,,,",
        "22,Amb L,LCL 21,44,Yes,No,80,CY,12,,,-10,-100,Yes,,Yes,22,Amb L,C414,Tall,room mics",
        "23,Amb R,LCL 22,44,Yes,No,80,CY,12,,,-10,100,Yes,,No,23,Amb R,C414,Tall,",
        "24,Shout,LCL 23,34,No,No,150,WH,10,,2,Off,0,No,,No,24,Shout,SM58,Tall,",
    ];
    let mut out = String::new();
    out.push_str(&reference());
    out.push_str(header);
    out.push('\n');
    for row in rows {
        out.push_str(row);
        out.push('\n');
    }
    out
}

/// The comment block at the top of the template. Lines beginning `#` are
/// skipped by the reader, so the documentation travels with the sheet.
fn reference() -> String {
    let lines: &[(&str, &str)] = &[
        ("Ch", "console channel: 1-40 on a WING, 1-16 on a Qu-16. The only\n#              column that must be filled in."),
        ("Name", "channel name, up to 16 characters."),
        ("Source", "the socket it arrives on: LCL 1, A 12, USB 3, SC 4, or Off."),
        ("Gain", "preamp gain in dB, -3 to 45.5. Written to the socket, not the channel."),
        ("48V", "phantom power: Yes/No (also Y, X, 1, True)."),
        ("Pol", "polarity invert: Yes/No."),
        ("LowCut", "low cut frequency in Hz, 20-2000, or Off."),
        ("Colour", "the console's 1-18, or a name: RED GREEN BLUE YELLOW CYAN MAGENTA\n#              ORANGE PURPLE TEAL LIME SALMON CORAL MINT INDIGO SKY BROWN\n#              GREY WHITE."),
        ("Icon", "console icon number, 0-999."),
        ("DCA", "DCA membership: 1, or 1,2 - up to 16."),
        ("MuteGrp", "mute group membership: 1-8, same format."),
        ("Fader", "fader position in dB, -144 to +10, or Off."),
        ("Pan", "-100 (left) to 100 (right)."),
        ("Main", "assign to the main bus: Yes/No."),
        ("Mute", "start muted: Yes/No."),
        ("Sends", "bus sends as bus:level, e.g. \"1:-6,3:0\". Add p for pre-fader."),
        ("Link", "Yes on the odd channel of a stereo pair."),
        ("Track", "which record output, and so which DAW track, carries this channel."),
        ("TrackName", "track name, when it should differ from the channel name."),
    ];
    let mut out = String::new();
    out.push_str("# Patch sheet. Fill it in, then:\n");
    out.push_str("#   wing-livetrax-bridge build --sheet this-file.csv --dest ~/Music/Livetrax --name \"My Show\"\n");
    out.push_str("#\n# That builds a WING snapshot and a LiveTrax session. For an Allen & Heath\n");
    out.push_str("# Qu, add --desk qu-16 (or qu-24, qu-32): a Qu has no published file format,\n");
    out.push_str("# so the sheet builds the session only, from Ch, Name, Colour, Track and\n");
    out.push_str("# TrackName. The rest is read and reported, not applied.\n");
    out.push_str("#\n# Columns are found by name and may appear in any order. An empty cell\n");
    out.push_str("# leaves that setting as the base snapshot had it, so a sheet with only\n");
    out.push_str("# Ch and Name changes only names. Columns not listed here - Mic, Stand,\n");
    out.push_str("# Notes - are carried through untouched. A title line above the header is\n");
    out.push_str("# fine; the header is found by looking for the Ch column.\n#\n");
    for (name, doc) in lines {
        out.push_str(&format!("#   {name:<10} {doc}\n"));
    }
    out.push_str("#\n# The rows below are an example: delete them and put your own patch in.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Row {
        parse(text).expect("sheet should parse").rows.remove(0)
    }

    #[test]
    fn a_sheet_needs_only_a_channel_and_a_name() {
        let sheet = parse("Ch,Name\n1,Kick\n2,Snare\n").unwrap();
        assert_eq!(sheet.rows.len(), 2);
        assert_eq!(sheet.rows[0].name.as_deref(), Some("Kick"));
        assert!(sheet.rows[0].source.is_none(), "an absent column stays None");
        assert!(sheet.rows[0].gain.is_none());
    }

    #[test]
    fn an_empty_cell_is_not_a_zero() {
        // The difference matters: a blank Gain must leave the preamp alone
        // rather than pulling it down to nothing.
        let row = one("Ch,Name,Gain,48V\n1,Kick,,\n");
        assert!(row.gain.is_none());
        assert!(row.phantom.is_none());
    }

    #[test]
    fn columns_are_found_by_name_in_any_order() {
        let row = one("Notes,48 V,NAME,ch,Low-Cut\nhello,Yes,Kick,7,80\n");
        assert_eq!(row.channel, 7);
        assert_eq!(row.name.as_deref(), Some("Kick"));
        assert_eq!(row.phantom, Some(true));
        assert_eq!(row.low_cut, Some(Some(80.0)));
        assert_eq!(row.extra.get("Notes").map(String::as_str), Some("hello"));
    }

    #[test]
    fn the_ways_a_spreadsheet_says_yes() {
        for yes in ["Yes", "y", "TRUE", "1", "x", "On", "✓"] {
            assert!(flag(yes).unwrap(), "{yes:?} should be yes");
        }
        for no in ["No", "n", "FALSE", "0", "off", "-"] {
            assert!(!flag(no).unwrap(), "{no:?} should be no");
        }
        assert!(flag("maybe").is_err());
    }

    #[test]
    fn numbers_survive_the_units_people_type() {
        assert_eq!(number("32 dB").unwrap(), 32.0);
        assert_eq!(number("+4").unwrap(), 4.0);
        assert_eq!(number("\u{2212}6").unwrap(), -6.0, "a word processor's minus sign");
        assert_eq!(level("Off").unwrap(), -144.0);
        assert_eq!(level("-\u{221e}").unwrap(), -144.0);
    }

    #[test]
    fn sources_are_read_however_they_are_written() {
        assert_eq!(source("LCL 1").unwrap(), SourceRef { group: "LCL".into(), index: 1 });
        assert_eq!(source("lcl12").unwrap(), SourceRef { group: "LCL".into(), index: 12 });
        assert_eq!(source("A 5").unwrap(), SourceRef { group: "A".into(), index: 5 });
        assert_eq!(source("Local 3").unwrap(), SourceRef { group: "LCL".into(), index: 3 });
        assert!(source("Off").unwrap().is_off());
        assert!(source("XLR 3").is_err(), "an unknown group should be named, not guessed at");
        assert!(source("LCL").is_err());
    }

    #[test]
    fn a_send_cell_carries_bus_level_and_tap() {
        let parsed = sends("1:-6, 3:0, 5:-10p").unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], Send { bus: 1, level: -6.0, pre: None });
        assert_eq!(parsed[2], Send { bus: 5, level: -10.0, pre: Some(true) });
        // A bare list means "on, at unity".
        assert_eq!(sends("2,4").unwrap()[0], Send { bus: 2, level: 0.0, pre: None });
        assert!(sends("1:-6,1:0").is_err(), "the same bus twice is a mistake worth naming");
        assert!(sends("17:0").is_err());
    }

    #[test]
    fn dca_cells_come_in_every_punctuation() {
        assert_eq!(indices("1", 16, "DCA").unwrap(), vec![1]);
        assert_eq!(indices("1,2", 16, "DCA").unwrap(), vec![1, 2]);
        assert_eq!(indices("DCA 3 / DCA 1", 16, "DCA").unwrap(), vec![1, 3]);
        assert!(indices("9", 8, "mute group").is_err());
    }

    #[test]
    fn colours_take_a_name_or_the_consoles_index() {
        assert_eq!(colour("RD").unwrap(), 9);
        assert_eq!(colour("red").unwrap(), 9);
        assert_eq!(colour("blue").unwrap(), 1);
        assert_eq!(colour("12").unwrap(), 12);
        assert!(colour("puce").is_err());
        // Every index the console offers has a name, and no two share one.
        let mut names: Vec<&str> = COLOURS.iter().flat_map(|(_, _, n)| n.iter().copied()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two colours answer to the same name");
        for (idx, _, _) in COLOURS {
            assert_eq!(colour(&idx.to_string()).unwrap(), idx);
        }
    }

    #[test]
    fn quoted_cells_hold_commas_and_comments_are_skipped() {
        let sheet = parse("# a note\nCh,Name,Sends\n1,\"Vox, lead\",\"1:-6,2:0\"\n").unwrap();
        assert_eq!(sheet.rows[0].name.as_deref(), Some("Vox, lead"));
        assert_eq!(sheet.rows[0].sends.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn tabs_and_semicolons_are_spreadsheets_too() {
        assert_eq!(parse("Ch\tName\n1\tKick\n").unwrap().rows[0].name.as_deref(), Some("Kick"));
        assert_eq!(parse("Ch;Name\n1;Kick\n").unwrap().rows[0].name.as_deref(), Some("Kick"));
    }

    #[test]
    fn a_mistake_says_which_line_and_which_column() {
        let err = parse("Ch,Name,Gain\n1,Kick,32\n2,Snare,99\n").unwrap_err().to_string();
        assert!(err.contains("line 3"), "{err}");
        assert!(err.contains("Gain"), "{err}");
    }

    #[test]
    fn a_sheet_with_no_channel_column_is_refused_by_name() {
        let err = parse("Name,Gain\nKick,32\n").unwrap_err().to_string();
        assert!(err.contains("channel column"), "{err}");
    }

    #[test]
    fn blank_rows_and_section_headings_are_not_channels() {
        let sheet = parse("Ch,Name\n1,Kick\n,DRUMS\n\n2,Snare\n").unwrap();
        assert_eq!(sheet.rows.len(), 2);
    }

    #[test]
    fn two_channels_on_one_track_is_worth_saying() {
        let sheet = parse("Ch,Name,Track\n1,Kick,1\n2,Snare,1\n").unwrap();
        assert!(
            sheet.warnings.iter().any(|w| w.contains("record to track 1")),
            "{:?}",
            sheet.warnings
        );
    }

    #[test]
    fn the_template_is_a_sheet_this_reader_accepts() {
        let sheet = parse(&template()).expect("the shipped template must parse");
        assert_eq!(sheet.rows.len(), 24);
        assert_eq!(sheet.unknown_columns, vec!["Mic", "Stand", "Notes"]);
        assert!(sheet.warnings.is_empty(), "{:?}", sheet.warnings);
        let kick = &sheet.rows[0];
        assert_eq!(kick.name.as_deref(), Some("Kick In"));
        assert_eq!(kick.source.as_ref().unwrap().to_string(), "LCL 1");
        assert_eq!(kick.gain, Some(32.0));
        assert_eq!(kick.dca.as_deref(), Some(&[1u8][..]));
        assert_eq!(kick.track, Some(1));
    }
}
