//! SMPTE timecode.
//!
//! The DAW publishes its playhead as a timecode string. Going the other way,
//! "locate to 01:02:03:04", means doing the arithmetic here and doing it
//! properly: the 29.97 and 59.94 rates run 1000/1001 slow, and their drop-frame
//! variants skip frame numbers to keep the clock honest against the wall.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fps {
    #[serde(rename = "23.976")]
    F23976,
    #[serde(rename = "24")]
    F24,
    #[serde(rename = "25")]
    F25,
    #[serde(rename = "29.97")]
    F2997,
    #[serde(rename = "29.97df")]
    F2997Drop,
    #[default]
    #[serde(rename = "30")]
    F30,
    #[serde(rename = "30df")]
    F30Drop,
    #[serde(rename = "59.94")]
    F5994,
    #[serde(rename = "59.94df")]
    F5994Drop,
    #[serde(rename = "60")]
    F60,
}

impl Fps {
    pub const ALL: [Fps; 10] = [
        Fps::F23976, Fps::F24, Fps::F25, Fps::F2997, Fps::F2997Drop,
        Fps::F30, Fps::F30Drop, Fps::F5994, Fps::F5994Drop, Fps::F60,
    ];

    /// Frame numbers per second of timecode, which is not the same as the rate.
    pub fn frames_per_second(self) -> i64 {
        match self {
            Fps::F23976 | Fps::F24 => 24,
            Fps::F25 => 25,
            Fps::F2997 | Fps::F2997Drop | Fps::F30 | Fps::F30Drop => 30,
            Fps::F5994 | Fps::F5994Drop | Fps::F60 => 60,
        }
    }

    /// Frames actually elapsed per second of real time.
    pub fn rate(self) -> f64 {
        match self {
            Fps::F23976 => 24_000.0 / 1001.0,
            Fps::F24 => 24.0,
            Fps::F25 => 25.0,
            Fps::F2997 | Fps::F2997Drop => 30_000.0 / 1001.0,
            Fps::F30 | Fps::F30Drop => 30.0,
            Fps::F5994 | Fps::F5994Drop => 60_000.0 / 1001.0,
            Fps::F60 => 60.0,
        }
    }

    pub fn is_drop(self) -> bool {
        matches!(self, Fps::F2997Drop | Fps::F30Drop | Fps::F5994Drop)
    }

    /// Frame numbers skipped at the top of a dropping minute.
    fn dropped_per_minute(self) -> i64 {
        if !self.is_drop() {
            0
        } else if self.frames_per_second() >= 60 {
            4
        } else {
            2
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Fps::F23976 => "23.976",
            Fps::F24 => "24",
            Fps::F25 => "25",
            Fps::F2997 => "29.97",
            Fps::F2997Drop => "29.97 df",
            Fps::F30 => "30",
            Fps::F30Drop => "30 df",
            Fps::F5994 => "59.94",
            Fps::F5994Drop => "59.94 df",
            Fps::F60 => "60",
        }
    }

    /// The value an Ardour-family session file stores, e.g. `timecode_2997drop`.
    pub fn from_session_value(raw: &str) -> Option<Fps> {
        Some(match raw.trim().trim_start_matches("timecode_") {
            "23976" => Fps::F23976,
            "24" => Fps::F24,
            "25" => Fps::F25,
            "2997" => Fps::F2997,
            "2997drop" => Fps::F2997Drop,
            "30" => Fps::F30,
            "30drop" => Fps::F30Drop,
            "5994" => Fps::F5994,
            "5994drop" => Fps::F5994Drop,
            "60" => Fps::F60,
            _ => return None,
        })
    }
}

/// A timecode position, as hours:minutes:seconds:frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timecode {
    pub hours: i64,
    pub minutes: i64,
    pub seconds: i64,
    pub frames: i64,
    pub drop: bool,
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Drop-frame is conventionally written with a semicolon before frames.
        let sep = if self.drop { ';' } else { ':' };
        write!(
            f,
            "{:02}:{:02}:{:02}{}{:02}",
            self.hours, self.minutes, self.seconds, sep, self.frames
        )
    }
}

impl Timecode {
    /// Accepts `01:02:03:04`, `1:2:3:4`, `01:02:03;04`, or `01.02.03.04`.
    pub fn parse(raw: &str) -> Option<Timecode> {
        let cleaned: String = raw
            .trim()
            .chars()
            .map(|c| if c == ';' || c == '.' || c == ',' { ':' } else { c })
            .collect();
        let parts: Vec<&str> = cleaned.split(':').filter(|p| !p.is_empty()).collect();
        if parts.len() != 4 {
            return None;
        }
        let mut values = [0i64; 4];
        for (slot, part) in values.iter_mut().zip(parts) {
            *slot = part.trim().parse::<i64>().ok()?;
            if *slot < 0 {
                return None;
            }
        }
        Some(Timecode {
            hours: values[0],
            minutes: values[1],
            seconds: values[2],
            frames: values[3],
            drop: raw.contains(';'),
        })
    }

    /// Frame number counted from 00:00:00:00, accounting for dropped numbers.
    pub fn to_frame_number(self, fps: Fps) -> i64 {
        let per_second = fps.frames_per_second();
        let straight = ((self.hours * 60 + self.minutes) * 60 + self.seconds) * per_second
            + self.frames;
        let dropped = fps.dropped_per_minute();
        if dropped == 0 {
            return straight;
        }
        let total_minutes = self.hours * 60 + self.minutes;
        straight - dropped * (total_minutes - total_minutes / 10)
    }

    pub fn from_frame_number(frame: i64, fps: Fps) -> Timecode {
        let per_second = fps.frames_per_second();
        let dropped = fps.dropped_per_minute();
        let mut frame = frame.max(0);

        if dropped > 0 {
            // Put the skipped frame numbers back: nine minutes in every ten
            // drop `dropped` numbers, and the tenth keeps them.
            let frames_per_ten_minutes = per_second * 600 - dropped * 9;
            let frames_per_minute = per_second * 60 - dropped;
            let tens = frame / frames_per_ten_minutes;
            let rest = frame % frames_per_ten_minutes;
            frame += dropped * 9 * tens;
            if rest > dropped {
                frame += dropped * ((rest - dropped) / frames_per_minute);
            }
        }

        let frames = frame % per_second;
        let total_seconds = frame / per_second;
        Timecode {
            hours: total_seconds / 3600,
            minutes: (total_seconds / 60) % 60,
            seconds: total_seconds % 60,
            frames,
            drop: fps.is_drop(),
        }
    }
}

/// Sample position -> timecode, including the session's start offset.
pub fn from_samples(samples: i64, sample_rate: f64, fps: Fps, offset_frames: i64) -> Timecode {
    let rate = if sample_rate > 1.0 { sample_rate } else { 48_000.0 };
    let frame = (samples as f64 / rate * fps.rate()).round() as i64 + offset_frames;
    Timecode::from_frame_number(frame.max(0), fps)
}

/// Timecode -> sample position. Returns None when it lands before the session.
pub fn to_samples(tc: Timecode, sample_rate: f64, fps: Fps, offset_frames: i64) -> Option<i64> {
    let rate = if sample_rate > 1.0 { sample_rate } else { 48_000.0 };
    let frame = tc.to_frame_number(fps) - offset_frames;
    if frame < 0 {
        return None;
    }
    Some((frame as f64 / fps.rate() * rate).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_people_type() {
        assert_eq!(
            Timecode::parse("01:02:03:04"),
            Some(Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4, drop: false })
        );
        assert_eq!(Timecode::parse("1:2:3:4").unwrap().frames, 4);
        assert!(Timecode::parse("01:02:03;04").unwrap().drop);
        assert_eq!(Timecode::parse("01:02:03"), None);
        assert_eq!(Timecode::parse("not a timecode"), None);
    }

    #[test]
    fn round_trips_through_samples() {
        for fps in Fps::ALL {
            let tc = Timecode::parse("01:23:45:10").unwrap();
            let samples = to_samples(tc, 48_000.0, fps, 0).unwrap();
            let back = from_samples(samples, 48_000.0, fps, 0);
            assert_eq!(
                (back.hours, back.minutes, back.seconds, back.frames),
                (tc.hours, tc.minutes, tc.seconds, tc.frames),
                "{} did not survive the trip",
                fps.label()
            );
        }
    }

    #[test]
    fn drop_frame_skips_the_right_numbers() {
        // 00:00:59:29 is followed by 00:01:00:02 at 29.97 drop.
        let last = Timecode::parse("00:00:59:29").unwrap().to_frame_number(Fps::F2997Drop);
        let next = Timecode::from_frame_number(last + 1, Fps::F2997Drop);
        assert_eq!((next.minutes, next.seconds, next.frames), (1, 0, 2));

        // ...but the tenth minute keeps its frames.
        let last = Timecode::parse("00:09:59:29").unwrap().to_frame_number(Fps::F2997Drop);
        let next = Timecode::from_frame_number(last + 1, Fps::F2997Drop);
        assert_eq!((next.minutes, next.seconds, next.frames), (10, 0, 0));
    }

    #[test]
    fn drop_frame_tracks_the_wall_clock() {
        // One hour of 29.97 drop is one hour of real time, near enough.
        let hour = Timecode::parse("01:00:00:00").unwrap();
        let samples = to_samples(hour, 48_000.0, Fps::F2997Drop, 0).unwrap();
        let seconds = samples as f64 / 48_000.0;
        assert!((seconds - 3600.0).abs() < 0.2, "an hour drifted to {seconds}s");

        // Non-drop 29.97 runs slow by 0.1%: an hour of timecode is longer.
        let samples = to_samples(hour, 48_000.0, Fps::F2997, 0).unwrap();
        let seconds = samples as f64 / 48_000.0;
        assert!((seconds - 3603.6).abs() < 0.2, "expected ~3603.6s, got {seconds}");
    }

    #[test]
    fn session_start_offset_shifts_the_clock() {
        let fps = Fps::F30;
        let offset = Timecode::parse("10:00:00:00").unwrap().to_frame_number(fps);
        // The very start of the session reads as the offset itself.
        assert_eq!(from_samples(0, 48_000.0, fps, offset).to_string(), "10:00:00:00");
        // ...and locating back to it lands on sample zero.
        let tc = Timecode::parse("10:00:00:00").unwrap();
        assert_eq!(to_samples(tc, 48_000.0, fps, offset), Some(0));
        // Anything before the session start has no sample position.
        let early = Timecode::parse("09:59:00:00").unwrap();
        assert_eq!(to_samples(early, 48_000.0, fps, offset), None);
    }

    #[test]
    fn reads_the_session_file_spelling() {
        assert_eq!(Fps::from_session_value("timecode_2997drop"), Some(Fps::F2997Drop));
        assert_eq!(Fps::from_session_value("timecode_25"), Some(Fps::F25));
        assert_eq!(Fps::from_session_value("timecode_30"), Some(Fps::F30));
        assert_eq!(Fps::from_session_value("nonsense"), None);
    }
}
