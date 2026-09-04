//! Behringer WING `.snap` files.
//!
//! A WING snapshot is JSON that mirrors the console's node tree, so reading one
//! is the offline way to learn the same things a live query would tell us:
//! `ae_data.ch.<n>.name` (channel names), `ae_data.ch.<n>.in.conn` (the input
//! each channel owns) and `ae_data.io.out.<GROUP>.<n>` (what feeds each
//! physical output). Everything is turned into a [`PatchModel`], which owns the
//! resolution rules and is shared with the live path.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::patch::{OutputGroup, PatchModel, Slot};

pub use crate::patch::group_label;

pub struct SnapFile {
    root: Value,
    /// Built once at load: the GUI asks for slots every frame.
    model: PatchModel,
    pub path: PathBuf,
}

impl SnapFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let root: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a WING .snap (JSON) file", path.display()))?;
        anyhow::ensure!(
            root.get("ae_data").is_some(),
            "{} has no ae_data section - is it a WING snapshot?",
            path.display()
        );
        let model = build_model(&root);
        Ok(Self { root, model, path: path.to_path_buf() })
    }

    /// "wing-compact, FLETCHER, 2025-10-24" - so you can tell snaps apart.
    pub fn creator(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for key in ["creator_model", "creator_name", "created"] {
            if let Some(v) = self.root.get(key).and_then(Value::as_str) {
                if !v.is_empty() {
                    parts.push(v.to_string());
                }
            }
        }
        parts.join(", ")
    }

    pub fn channel_names(&self) -> BTreeMap<u16, String> {
        self.model.channel_names.clone()
    }

    pub fn output_groups(&self) -> Vec<OutputGroup> {
        self.model.groups()
    }

    pub fn outputs(&self, group: &str) -> Vec<Slot> {
        self.model.slots(group)
    }

    pub fn channel_count(&self) -> usize {
        self.root
            .get("ae_data")
            .and_then(|v| v.get("ch"))
            .and_then(Value::as_object)
            .map(|c| c.len())
            .unwrap_or(0)
    }

    /// Rename a channel in place. Only that channel's `name` is touched.
    pub fn set_channel_name(&mut self, ch: u16, name: &str) -> Result<()> {
        let source = self.path.display().to_string();
        let entry = self
            .root
            .get_mut("ae_data")
            .and_then(|v| v.get_mut("ch"))
            .and_then(|v| v.get_mut(ch.to_string()))
            .with_context(|| format!("{source} has no channel {ch}"))?;
        entry["name"] = Value::String(name.to_string());
        self.model.channel_names.insert(ch, name.to_string());
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string(&self.root).context("serialising snapshot")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Pull the patch out of the snapshot's JSON.
fn build_model(root: &Value) -> PatchModel {
    let mut model = PatchModel::default();
    let Some(ae) = root.get("ae_data") else { return model };

    if let Some(channels) = ae.get("ch").and_then(Value::as_object) {
        for (key, value) in channels {
            let Ok(ch) = key.parse::<u16>() else { continue };
            if let Some(name) = value.get("name").and_then(Value::as_str) {
                if !name.trim().is_empty() {
                    model.channel_names.insert(ch, name.trim().to_string());
                }
            }
            if let Some(conn) = value.get("in").and_then(|i| i.get("conn")) {
                let grp = conn.get("grp").and_then(Value::as_str).unwrap_or("OFF");
                let index = conn.get("in").and_then(Value::as_i64).unwrap_or(0) as u16;
                model.channel_inputs.insert(ch, (grp.to_string(), index));
            }
        }
    }

    for section in ["aux", "bus", "main", "mtx", "fx", "dca"] {
        let Some(entries) = ae.get(section).and_then(Value::as_object) else { continue };
        for (key, value) in entries {
            let (Ok(n), Some(name)) = (key.parse::<u16>(), value.get("name").and_then(Value::as_str))
            else {
                continue;
            };
            if !name.trim().is_empty() {
                model
                    .object_names
                    .insert((section.to_string(), n), name.trim().to_string());
            }
        }
    }

    if let Some(inputs) = ae.get("io").and_then(|v| v.get("in")).and_then(Value::as_object) {
        for (group, entries) in inputs {
            let Some(entries) = entries.as_object() else { continue };
            for (key, value) in entries {
                let (Ok(n), Some(name)) =
                    (key.parse::<u16>(), value.get("name").and_then(Value::as_str))
                else {
                    continue;
                };
                if !name.trim().is_empty() {
                    model
                        .input_names
                        .insert((group.clone(), n), name.trim().to_string());
                }
            }
        }
    }

    if let Some(outputs) = ae.get("io").and_then(|v| v.get("out")).and_then(Value::as_object) {
        for (group, entries) in outputs {
            let Some(entries) = entries.as_object() else { continue };
            let mut slots = BTreeMap::new();
            for (key, value) in entries {
                let Ok(n) = key.parse::<u16>() else { continue };
                let grp = value.get("grp").and_then(Value::as_str).unwrap_or("OFF");
                let index = value.get("in").and_then(Value::as_i64).unwrap_or(0) as u16;
                slots.insert(n, (grp.to_string(), index));
            }
            if !slots.is_empty() {
                model.outputs.insert(group.clone(), slots);
            }
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature snapshot with the same shape as a console's: channels that
    /// own local inputs, a stereo matrix and main, and a USB output patch that
    /// is deliberately not 1:1.
    const SNAP: &str = r#"{
      "type": "snapshot.10",
      "creator_model": "wing-compact",
      "creator_name": "TEST",
      "created": "2026-01-01 00:00:00",
      "ae_data": {
        "ch": {
          "1": {"name": "KICK",  "in": {"conn": {"grp": "LCL", "in": 1}}},
          "2": {"name": "SNARE", "in": {"conn": {"grp": "LCL", "in": 2}}},
          "3": {"name": "VOX",   "in": {"conn": {"grp": "LCL", "in": 5}}},
          "4": {"name": "",      "in": {"conn": {"grp": "OFF", "in": 1}}}
        },
        "bus":  {"1": {"name": "MON 1"}, "2": {"name": ""}},
        "main": {"1": {"name": "PA"}},
        "mtx":  {"1": {"name": "REC"}},
        "io": {
          "in": {"LCL": {"1": {"name": ""}, "2": {"name": ""},
                         "3": {"name": "TALKBACK"}, "5": {"name": ""}}},
          "out": {
            "USB": {"1": {"grp": "LCL", "in": 1}, "2": {"grp": "LCL", "in": 2},
                    "3": {"grp": "LCL", "in": 3}, "4": {"grp": "LCL", "in": 5},
                    "5": {"grp": "MTX", "in": 1}, "6": {"grp": "MTX", "in": 2},
                    "7": {"grp": "OFF", "in": 1}},
            "LCL": {"1": {"grp": "MAIN", "in": 1}, "2": {"grp": "MAIN", "in": 2}}
          }
        }
      }
    }"#;

    fn fixture() -> (SnapFile, tempdir::Dir) {
        let dir = tempdir::Dir::new("snapfile");
        let path = dir.path().join("test.snap");
        std::fs::write(&path, SNAP).unwrap();
        (SnapFile::load(&path).unwrap(), dir)
    }

    /// Minimal scratch directory helper.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "wltb-{tag}-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn lists_output_groups_by_how_much_is_patched() {
        let (snap, _dir) = fixture();
        let groups = snap.output_groups();
        assert_eq!(groups[0].id, "USB");
        assert_eq!(groups[0].patched, 6, "the OFF output must not count");
        assert_eq!(groups[0].outputs, 7);
        assert_eq!(groups[0].label, "USB audio");
    }

    #[test]
    fn outputs_resolve_through_the_input_patch() {
        let (snap, _dir) = fixture();
        let slots = snap.outputs("USB");
        let named: Vec<(u16, Option<u16>, &str)> = slots
            .iter()
            .map(|s| (s.output, s.channel, s.name.as_str()))
            .collect();
        assert_eq!(
            named,
            vec![
                (1, Some(1), "KICK"),
                (2, Some(2), "SNARE"),
                // No channel owns local input 3, so the socket's own label wins.
                (3, None, "TALKBACK"),
                // Channel 3 lives on local input 5, not 4: this is the case a
                // straight channel-to-track mapping gets wrong.
                (4, Some(3), "VOX"),
                // Mix objects are patched per leg.
                (5, None, "REC L"),
                (6, None, "REC R"),
                (7, None, ""),
            ]
        );
    }

    #[test]
    fn stereo_mains_resolve_per_leg() {
        let (snap, _dir) = fixture();
        let slots = snap.outputs("LCL");
        assert_eq!(slots[0].name, "PA L");
        assert_eq!(slots[1].name, "PA R");
    }

    #[test]
    fn renaming_touches_only_the_name() {
        let (mut snap, dir) = fixture();
        snap.set_channel_name(2, "Snare Top").unwrap();
        assert!(snap.set_channel_name(99, "nope").is_err());
        let out = dir.path().join("out.snap");
        snap.save(&out).unwrap();

        let before: serde_json::Value = serde_json::from_str(SNAP).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(after["ae_data"]["ch"]["2"]["name"], "Snare Top");
        // Everything else is byte-identical in meaning.
        let mut expected = before.clone();
        expected["ae_data"]["ch"]["2"]["name"] = serde_json::Value::String("Snare Top".into());
        assert_eq!(after, expected);
    }

    #[test]
    fn channel_map_follows_the_output_patch() {
        let (snap, _dir) = fixture();
        let map = crate::config::ChannelMap::from_outputs(&snap.outputs("USB"), 0);
        // Strip 4 is fed by console channel 3.
        assert_eq!(map.strip(3), Some(4));
        assert_eq!(map.channel(4), Some(3));
        // Outputs carrying a bus or nothing map to no channel.
        assert_eq!(map.channel(5), None);
        assert_eq!(map.len(), 3);
    }
}
