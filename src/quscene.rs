//! Allen & Heath Qu scene files (`.DAT`).
//!
//! A Qu writes its scenes to USB as fixed-layout binary. The layout is not
//! published: what is here follows the community work in
//! <https://github.com/crossan007/AHQUToolkit>, whose decoder reads the same
//! offsets to produce readable scene dumps. Only the parts that matter for
//! building a session are read - the scene's name and the channels' - and
//! everything is checked on the way in rather than trusted, because a format
//! nobody publishes is a format that can change under you.
//!
//! ```text
//! 0x03            scene number
//! 0x0C..0x17      scene name, NUL-terminated
//! 0x30            first channel, then 60 channels of 0xC0 bytes each
//!   +0x9C..0xA4     channel name, NUL-terminated
//!   +0xB7           channel number
//! ```
//!
//! Channels 1-32 are the mono inputs (a Qu-16 uses the first 16 of them),
//! 33-35 the stereo inputs, and 36-39 the FX returns.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const SCENE_ID: usize = 0x03;
const SCENE_NAME: std::ops::Range<usize> = 0x0C..0x17;
const CHANNELS_AT: usize = 0x30;
const CHANNEL_SIZE: usize = 0xC0;
const CHANNEL_COUNT: usize = 60;
const NAME_AT: usize = 0x9C;
const NAME_LEN: usize = 8;
const ID_AT: usize = 0xB7;

/// Everything up to the end of the channel block, which is all this reads.
const NEEDED: usize = CHANNELS_AT + CHANNEL_COUNT * CHANNEL_SIZE;
/// A whole scene, including the parts this does not read.
const WHOLE: usize = 0x6520;

/// What a channel slot is on the desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Mono input, 1-32.
    Input(u16),
    /// Stereo input, 1-3.
    Stereo(u16),
    /// FX return, 1-4.
    FxReturn(u16),
    /// A slot in the file the desk uses for something else.
    Other,
}

impl Slot {
    fn of(index: usize) -> Slot {
        match index {
            0..=31 => Slot::Input(index as u16 + 1),
            32..=34 => Slot::Stereo(index as u16 - 31),
            35..=38 => Slot::FxReturn(index as u16 - 34),
            _ => Slot::Other,
        }
    }

    pub fn label(self) -> String {
        match self {
            Slot::Input(n) => format!("Ch {n}"),
            Slot::Stereo(n) => format!("ST{n}"),
            Slot::FxReturn(n) => format!("FX{n} return"),
            Slot::Other => "-".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Channel {
    /// Position in the file's channel block.
    pub index: usize,
    /// The channel number the desk stores with it.
    pub id: u8,
    /// The name on the desk, empty when it was never set.
    pub name: String,
    pub slot: Slot,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub name: String,
    pub id: u8,
    pub channels: Vec<Channel>,
    pub path: PathBuf,
    /// Said when the file is shorter than a whole scene but long enough to
    /// read: worth knowing, not worth refusing.
    pub warnings: Vec<String>,
}

pub fn read(path: &Path) -> Result<Scene> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut scene = parse(&bytes, &path.display().to_string())?;
    scene.path = path.to_path_buf();
    Ok(scene)
}

pub fn parse(bytes: &[u8], source: &str) -> Result<Scene> {
    if bytes.len() < NEEDED {
        bail!(
            "{source} is {} bytes; a Qu scene is at least {NEEDED}. Copy the .DAT file \
             straight off the USB stick - a scene is not the show folder around it.",
            bytes.len()
        );
    }
    let mut warnings = Vec::new();
    if bytes.len() < WHOLE {
        warnings.push(format!(
            "{source} is {} bytes rather than the usual {WHOLE}; the channel names read, but \
             the rest of the file may be truncated",
            bytes.len()
        ));
    }

    let mut channels = Vec::with_capacity(CHANNEL_COUNT);
    let mut suspicious = 0;
    for index in 0..CHANNEL_COUNT {
        let at = CHANNELS_AT + index * CHANNEL_SIZE;
        let raw = &bytes[at + NAME_AT..at + NAME_AT + NAME_LEN];
        let name = match text(raw) {
            Some(name) => name,
            None => {
                // A name that is not text is the clearest sign this is not a
                // scene file - or not one this layout describes.
                if index < 16 {
                    suspicious += 1;
                }
                String::new()
            }
        };
        channels.push(Channel {
            index,
            id: bytes[at + ID_AT],
            name,
            slot: Slot::of(index),
        });
    }
    if suspicious > 8 {
        bail!(
            "{source} does not read as a Qu scene: {suspicious} of the first 16 channel names \
             are not text. If it came off a Qu, the layout may have changed - the names are \
             read at 0x9C in each 0xC0-byte channel."
        );
    }

    Ok(Scene {
        name: text(&bytes[SCENE_NAME]).unwrap_or_default(),
        id: bytes[SCENE_ID],
        channels,
        path: PathBuf::new(),
        warnings,
    })
}

/// A NUL-terminated, printable name, or nothing if it is not text.
fn text(raw: &[u8]) -> Option<String> {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    let body = &raw[..end];
    if body.iter().any(|b| !(0x20..=0x7E).contains(b)) {
        return None;
    }
    Some(String::from_utf8_lossy(body).trim().to_string())
}

impl Scene {
    /// The stereo inputs and FX returns, for when they are recorded too. The
    /// name is empty where the scene never set one, so the caller can decide
    /// whether an unnamed return is worth a track.
    pub fn extras(&self) -> Vec<(String, String)> {
        self.channels
            .iter()
            .filter(|c| matches!(c.slot, Slot::Stereo(_) | Slot::FxReturn(_)))
            .map(|c| (c.slot.label(), c.name.clone()))
            .collect()
    }

    pub fn named(&self) -> usize {
        self.channels.iter().filter(|c| !c.name.is_empty()).count()
    }

    pub fn summary(&self) -> String {
        format!(
            "scene {} \"{}\", {} channels named",
            self.id,
            self.name,
            self.named()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scene file laid out the way the format describes.
    fn scene_file(names: &[(usize, &str)], scene_name: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; WHOLE];
        bytes[SCENE_ID] = 7;
        bytes[SCENE_NAME.start..SCENE_NAME.start + scene_name.len()]
            .copy_from_slice(scene_name.as_bytes());
        for (index, name) in names {
            let at = CHANNELS_AT + index * CHANNEL_SIZE;
            bytes[at + NAME_AT..at + NAME_AT + name.len()].copy_from_slice(name.as_bytes());
            bytes[at + ID_AT] = *index as u8 + 1;
        }
        bytes
    }

    #[test]
    fn reads_the_names_off_a_scene() {
        let bytes = scene_file(
            &[(0, "Kick"), (1, "Snare"), (15, "Talkbck"), (32, "Playback"), (35, "Verb")],
            "FRIDAY",
        );
        let scene = parse(&bytes, "test").unwrap();
        assert_eq!(scene.name, "FRIDAY");
        assert_eq!(scene.id, 7);
        assert_eq!(scene.named(), 5);

        let named: Vec<(String, String)> = scene
            .channels
            .iter()
            .filter(|c| !c.name.is_empty())
            .map(|c| (c.slot.label(), c.name.clone()))
            .collect();
        assert_eq!(named[0], ("Ch 1".into(), "Kick".into()));
        assert_eq!(named[2], ("Ch 16".into(), "Talkbck".into()));

        // The stereo inputs and FX returns are kept apart from the mono ones,
        // and say which of them the scene actually named.
        let extras = scene.extras();
        assert_eq!(extras.len(), 7, "three stereo inputs and four FX returns");
        assert_eq!(extras[0], ("ST1".into(), "Playback".into()));
        assert_eq!(extras[1], ("ST2".into(), String::new()), "never named");
        assert_eq!(extras[3], ("FX1 return".into(), "Verb".into()));
    }

    #[test]
    fn the_mono_inputs_are_the_first_thirty_two_slots() {
        let bytes = scene_file(&[(0, "Kick"), (20, "Spare")], "SHOW");
        let scene = parse(&bytes, "test").unwrap();
        let inputs: Vec<&Channel> = scene
            .channels
            .iter()
            .filter(|c| matches!(c.slot, Slot::Input(_)))
            .collect();
        assert_eq!(inputs.len(), 32);
        assert_eq!(inputs[0].name, "Kick");
        assert_eq!(inputs[20].name, "Spare", "a Qu-24 or -32 has this one");
        assert_eq!(inputs[1].name, "", "never named");
    }

    #[test]
    fn a_short_file_says_what_it_wanted() {
        let err = parse(&[0u8; 128], "tiny.DAT").unwrap_err().to_string();
        assert!(err.contains("at least"), "{err}");
        assert!(err.contains("tiny.DAT"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_a_scene_is_refused() {
        // Something else of the right size: the names come out as rubbish, and
        // guessing would be worse than saying so.
        let mut bytes = vec![0xABu8; WHOLE];
        bytes[SCENE_NAME.start..SCENE_NAME.end].fill(0);
        let err = parse(&bytes, "photo.jpg").unwrap_err().to_string();
        assert!(err.contains("does not read as a Qu scene"), "{err}");
    }

    #[test]
    fn a_truncated_scene_still_reads_but_says_so() {
        let mut bytes = scene_file(&[(0, "Kick")], "SHOW");
        bytes.truncate(NEEDED);
        let scene = parse(&bytes, "short.DAT").unwrap();
        assert_eq!(scene.channels[0].name, "Kick");
        assert!(scene.warnings.iter().any(|w| w.contains("truncated")), "{:?}", scene.warnings);
    }
}
