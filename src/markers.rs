//! Marker table.
//!
//! LiveTrax/Ardour do not expose the location list over OSC, so positions come
//! from the session XML on disk. Markers created after the last save are picked
//! up live from `/marker` feedback plus the playhead position, so the table
//! stays useful between saves.

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub name: String,
    /// Position in samples.
    pub start: i64,
    /// True when learned at runtime rather than read from the session file.
    pub observed: bool,
}

/// What a session file tells us beyond its markers.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub markers: Vec<Marker>,
    pub sample_rate: Option<f64>,
    pub fps: Option<crate::timecode::Fps>,
    /// Session start offset, in samples, sign already applied.
    pub offset_samples: i64,
}

#[derive(Debug, Default, Clone)]
pub struct MarkerTable {
    markers: Vec<Marker>,
    pub sample_rate: Option<f64>,
}

impl MarkerTable {
    pub fn find(&self, name: &str) -> Option<&Marker> {
        self.markers
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(name))
    }

    pub fn names(&self) -> Vec<&str> {
        self.markers.iter().map(|m| m.name.as_str()).collect()
    }

    pub fn len(&self) -> usize { self.markers.len() }
    pub fn iter(&self) -> impl Iterator<Item = &Marker> { self.markers.iter() }

    /// Record a marker seen at runtime. Session-file entries win on conflict.
    pub fn observe(&mut self, name: &str, start: i64) {
        if let Some(existing) = self
            .markers
            .iter_mut()
            .find(|m| m.name.eq_ignore_ascii_case(name))
        {
            if existing.observed {
                existing.start = start;
            }
            return;
        }
        self.markers.push(Marker { name: name.to_string(), start, observed: true });
        self.markers.sort_by_key(|m| m.start);
    }

    /// Replace file-sourced markers, keeping runtime observations that the
    /// file does not mention.
    pub fn replace_from_file(&mut self, mut fresh: Vec<Marker>, sample_rate: Option<f64>) {
        let kept: Vec<Marker> = self
            .markers
            .iter()
            .filter(|m| m.observed && !fresh.iter().any(|f| f.name.eq_ignore_ascii_case(&m.name)))
            .cloned()
            .collect();
        fresh.extend(kept);
        fresh.sort_by_key(|m| m.start);
        self.markers = fresh;
        if sample_rate.is_some() {
            self.sample_rate = sample_rate;
        }
    }
}

/// Parse the parts of an Ardour/LiveTrax session file the bridge needs:
/// markers, sample rate, and the timecode format and offset.
pub fn parse_session(path: &Path) -> Result<SessionInfo> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading session file {}", path.display()))?;
    let mut reader = Reader::from_str(&text);
    // Positions are converted once the sample rate is known; the rate lives on
    // the root element, so it is always read before any <Location>.
    let mut raw_markers: Vec<(String, RawPos)> = Vec::new();
    let mut sample_rate = None;
    let mut fps = None;
    let mut offset_samples: i64 = 0;
    let mut offset_negative = false;

    loop {
        match reader.read_event().context("parsing session XML")? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name();
                match name.as_ref() {
                    b"Session" => {
                        if let Some(v) = attr(&e, b"sample-rate") {
                            sample_rate = v.parse::<f64>().ok();
                        }
                    }
                    b"Option" => {
                        let (Some(name), Some(value)) = (attr(&e, b"name"), attr(&e, b"value"))
                        else {
                            continue;
                        };
                        match name.as_str() {
                            "timecode-format" => {
                                fps = crate::timecode::Fps::from_session_value(&value)
                            }
                            "timecode-offset" => {
                                offset_samples = value.trim().parse::<i64>().unwrap_or(0)
                            }
                            "timecode-offset-negative" => {
                                offset_negative = matches!(value.trim(), "1" | "yes" | "true")
                            }
                            _ => {}
                        }
                    }
                    b"Location" => {
                        let flags = attr(&e, b"flags").unwrap_or_default();
                        // Marks only: ranges, loop and punch points are skipped.
                        if !flags.split(',').any(|f| f.trim() == "IsMark") {
                            continue;
                        }
                        let Some(label) = attr(&e, b"name") else { continue };
                        let Some(start) = attr(&e, b"start").and_then(|s| parse_pos(&s)) else {
                            continue;
                        };
                        raw_markers.push((label, start));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let rate = sample_rate.unwrap_or(48_000.0);
    let mut skipped = 0;
    let mut markers: Vec<Marker> = Vec::new();
    for (name, raw) in raw_markers {
        match raw.to_samples(rate) {
            Some(start) => markers.push(Marker { name, start, observed: false }),
            // Music-time markers need the tempo map, which is not parsed here.
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!("{skipped} music-time markers skipped: only audio-time markers can be located");
    }
    markers.sort_by_key(|m| m.start);
    Ok(SessionInfo {
        markers,
        sample_rate,
        fps,
        offset_samples: if offset_negative { -offset_samples } else { offset_samples },
    })
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == key).and_then(|a| {
        a.unescape_value().ok().map(|v| v.into_owned())
    })
}

/// Ardour's fixed timeline tick rate: chosen so every common sample rate
/// divides it exactly.
const SUPERCLOCK_TICKS_PER_SECOND: i64 = 282_240_000;

/// A position as the session file stores it.
enum RawPos {
    /// Ardour 6 and earlier: a plain sample count.
    Samples(i64),
    /// Ardour 7+ audio domain (`a` prefix): superclock ticks, *not* samples.
    Superclock(i64),
    /// Ardour 7+ music domain (`b` prefix): needs the tempo map to place.
    Beats,
}

impl RawPos {
    fn to_samples(&self, sample_rate: f64) -> Option<i64> {
        match self {
            RawPos::Samples(v) => Some(*v),
            RawPos::Superclock(ticks) => {
                Some((*ticks as f64 * sample_rate / SUPERCLOCK_TICKS_PER_SECOND as f64) as i64)
            }
            RawPos::Beats => None,
        }
    }
}

fn parse_pos(raw: &str) -> Option<RawPos> {
    let trimmed = raw.trim();
    match trimmed.chars().next()? {
        'a' | 'A' => trimmed[1..].parse::<i64>().ok().map(RawPos::Superclock),
        'b' | 'B' => Some(RawPos::Beats),
        c if c.is_ascii_digit() || c == '-' => trimmed.parse::<i64>().ok().map(RawPos::Samples),
        _ => None,
    }
}

/// Guess the session file for a session directory, if given one.
pub fn resolve_session_path(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if path.is_dir() {
        let mut best: Option<PathBuf> = None;
        for entry in std::fs::read_dir(path)? {
            let p = entry?.path();
            if p.extension().and_then(|e| e.to_str()) == Some("ardour") {
                // Prefer <dir>/<dirname>.ardour over backups.
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                let dir_name = path.file_name().and_then(|s| s.to_str()).unwrap_or_default();
                if stem == dir_name {
                    return Ok(p);
                }
                best.get_or_insert(p);
            }
        }
        if let Some(p) = best {
            return Ok(p);
        }
    }
    anyhow::bail!("no .ardour session file found at {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_domain_positions_are_superclock_not_samples() {
        // Taken from a real LiveTrax 3 session recorded at 96 kHz, where this
        // marker sits ~31 minutes in.
        let pos = parse_pos("a525171791340").unwrap();
        let samples = pos.to_samples(96_000.0).unwrap();
        let seconds = samples as f64 / 96_000.0;
        assert!((seconds - 1860.6).abs() < 1.0, "got {seconds}s");
    }

    #[test]
    fn bare_positions_are_samples() {
        assert_eq!(parse_pos("48000").unwrap().to_samples(48_000.0), Some(48_000));
    }

    #[test]
    fn music_time_positions_are_skipped() {
        assert!(parse_pos("b1920").unwrap().to_samples(48_000.0).is_none());
    }
}
