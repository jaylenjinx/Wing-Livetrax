//! The console's output patch, however it was obtained.
//!
//! A `.snap` file and a live console describe the same thing in the same shape:
//! channel names, the input each channel owns, the names of the mix objects,
//! and what is patched to each physical output. [`PatchModel`] holds that, and
//! owns the resolution rules, so a snapshot and a live query cannot drift
//! apart.

use std::collections::BTreeMap;

/// One port group that can be recorded from.
#[derive(Debug, Clone)]
pub struct OutputGroup {
    pub id: String,
    pub label: String,
    pub outputs: usize,
    /// Outputs with something patched to them.
    pub patched: usize,
}

impl OutputGroup {
    pub fn summary(&self) -> String {
        format!("{} - {} of {} patched", self.label, self.patched, self.outputs)
    }
}

/// One output of a group, and the name it should give a recorded track.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub output: u16,
    /// The patch as the console states it, e.g. "LCL 7" or "MAIN 2".
    pub source: String,
    /// Resolved name, empty when the output is unpatched.
    pub name: String,
    /// Console channel behind this output, when there is one.
    pub channel: Option<u16>,
}

/// A source reference as the console writes it: a group and a 1-based index.
pub type Ref = (String, u16);

#[derive(Debug, Clone, Default)]
pub struct PatchModel {
    pub channel_names: BTreeMap<u16, String>,
    /// Channel -> the physical input it takes.
    pub channel_inputs: BTreeMap<u16, Ref>,
    /// ("bus", 3) -> "M3", for mix objects.
    pub object_names: BTreeMap<Ref, String>,
    /// ("LCL", 3) -> "TALKBACK", when the console labels a socket.
    pub input_names: BTreeMap<Ref, String>,
    /// Group -> output number -> what feeds it.
    pub outputs: BTreeMap<String, BTreeMap<u16, Ref>>,
}

/// Mix objects are stereo and the patch addresses them one leg at a time:
/// `MAIN 1..8` is four stereo mains. Channels and DCAs index directly.
fn section_for(group: &str) -> Option<(&'static str, bool)> {
    Some(match group {
        "CH" => ("ch", false),
        "DCA" => ("dca", false),
        "AUX" => ("aux", true),
        "BUS" => ("bus", true),
        "MAIN" => ("main", true),
        "MTX" => ("mtx", true),
        "FX" => ("fx", true),
        _ => return None,
    })
}

/// Sections whose names this model wants, for live queries.
pub const OBJECT_SECTIONS: [(&str, &str, u16); 5] = [
    ("AUX", "aux", 8),
    ("BUS", "bus", 16),
    ("MAIN", "main", 4),
    ("MTX", "mtx", 8),
    ("FX", "fx", 16),
];

impl PatchModel {
    /// Port groups with outputs, most-patched first.
    pub fn groups(&self) -> Vec<OutputGroup> {
        let mut groups: Vec<OutputGroup> = self
            .outputs
            .iter()
            .map(|(id, entries)| OutputGroup {
                id: id.clone(),
                label: group_label(id),
                outputs: entries.len(),
                patched: entries.values().filter(|(grp, _)| grp != "OFF").count(),
            })
            .collect();
        groups.sort_by(|a, b| b.patched.cmp(&a.patched).then(a.id.cmp(&b.id)));
        groups
    }

    /// The outputs of one group, in order, with names resolved.
    pub fn slots(&self, group: &str) -> Vec<Slot> {
        let Some(entries) = self.outputs.get(group) else { return Vec::new() };
        let by_input = self.channels_by_input();
        entries
            .iter()
            .map(|(output, (grp, index))| {
                if grp == "OFF" {
                    return Slot {
                        output: *output,
                        source: "off".into(),
                        name: String::new(),
                        channel: None,
                    };
                }
                let channel = by_input.get(&(grp.clone(), *index)).copied();
                let name = match channel {
                    Some(ch) => self.channel_name(ch),
                    None => self.source_name(grp, *index),
                };
                Slot { output: *output, source: format!("{grp} {index}"), name, channel }
            })
            .collect()
    }

    /// Physical inputs behind a group's outputs that no channel owns, so the
    /// console's own socket labels are worth asking for.
    pub fn unclaimed_sockets(&self, group: &str) -> Vec<Ref> {
        let owned: std::collections::HashSet<Ref> = self
            .channel_inputs
            .values()
            .filter(|(grp, _)| grp != "OFF")
            .cloned()
            .collect();
        let mut wanted: Vec<Ref> = self
            .outputs
            .get(group)
            .map(|entries| {
                entries
                    .values()
                    .filter(|(grp, _)| grp != "OFF" && section_for(grp).is_none())
                    .filter(|source| !owned.contains(*source))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        wanted.sort();
        wanted.dedup();
        wanted
    }

    fn channel_name(&self, ch: u16) -> String {
        self.channel_names
            .get(&ch)
            .filter(|n| !n.trim().is_empty())
            .map(|n| n.trim().to_string())
            .unwrap_or_else(|| format!("CH {ch}"))
    }

    /// Which console channel takes its input from a given physical input.
    fn channels_by_input(&self) -> BTreeMap<Ref, u16> {
        let mut map: BTreeMap<Ref, u16> = BTreeMap::new();
        for (ch, source) in &self.channel_inputs {
            if source.0 == "OFF" {
                continue;
            }
            // First channel wins if one input feeds several.
            map.entry(source.clone()).or_insert(*ch);
        }
        map
    }

    /// Name for a source no channel picked up: a mix object, or the console's
    /// own label for a socket.
    fn source_name(&self, grp: &str, index: u16) -> String {
        let Some((section, stereo)) = section_for(grp) else {
            return self
                .input_names
                .get(&(grp.to_string(), index))
                .filter(|n| !n.trim().is_empty())
                .map(|n| n.trim().to_string())
                .unwrap_or_else(|| format!("{grp} {index}"));
        };
        let name_of = |n: u16| -> Option<String> {
            self.object_names
                .get(&(section.to_string(), n))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        if !stereo {
            // A channel's own output carries the channel's name.
            if section == "ch" {
                if let Some(name) = self.channel_names.get(&index) {
                    if !name.trim().is_empty() {
                        return name.trim().to_string();
                    }
                }
            }
            return name_of(index).unwrap_or_else(|| format!("{grp} {index}"));
        }
        let object = (index - 1) / 2 + 1;
        let side = if index % 2 == 1 { "L" } else { "R" };
        match name_of(object) {
            Some(name) => format!("{name} {side}"),
            None => format!("{grp} {object} {side}"),
        }
    }
}


/// Record a console reply that belongs to the patch. Returns true when the
/// message was one of ours.
pub fn absorb(
    model: &mut PatchModel,
    live: &crate::config::LivePatch,
    addr: &str,
    args: &[rosc::OscType],
) -> bool {
    use crate::osc;
    let text = args.first().and_then(osc::as_str).map(str::to_string);
    let number = args.first().and_then(osc::as_i64);

    if let Some((group, n)) = osc::parse_pair(&live.out_source_group, "grp", "n", addr) {
        if let Some(value) = text {
            model
                .outputs
                .entry(group)
                .or_default()
                .entry(n)
                .or_insert_with(|| ("OFF".into(), 0))
                .0 = value;
            return true;
        }
    }
    if let Some((group, n)) = osc::parse_pair(&live.out_source_index, "grp", "n", addr) {
        if let Some(value) = number {
            model
                .outputs
                .entry(group)
                .or_default()
                .entry(n)
                .or_insert_with(|| ("OFF".into(), 0))
                .1 = value as u16;
            return true;
        }
    }
    if let Some(ch) = osc::parse_template(&live.channel_input_group, "ch", addr) {
        if let Some(value) = text {
            model
                .channel_inputs
                .entry(ch as u16)
                .or_insert_with(|| ("OFF".into(), 0))
                .0 = value;
            return true;
        }
    }
    if let Some(ch) = osc::parse_template(&live.channel_input_index, "ch", addr) {
        if let Some(value) = number {
            model
                .channel_inputs
                .entry(ch as u16)
                .or_insert_with(|| ("OFF".into(), 0))
                .1 = value as u16;
            return true;
        }
    }
    if let Some((group, n)) = osc::parse_pair(&live.input_name, "grp", "n", addr) {
        if let Some(value) = text {
            if !value.trim().is_empty() {
                model.input_names.insert((group, n), value);
            }
            return true;
        }
    }
    if let Some((section, n)) = osc::parse_pair(&live.object_name, "sect", "n", addr) {
        if let Some(value) = text {
            if !value.trim().is_empty() {
                model.object_names.insert((section, n), value);
            }
            return true;
        }
    }
    false
}

/// Human names for WING port groups.
pub fn group_label(id: &str) -> String {
    match id {
        "LCL" => "Local outputs",
        "AUX" => "Aux outputs",
        "A" => "AES50-A",
        "B" => "AES50-B",
        "C" => "AES50-C",
        "SC" => "StageConnect",
        "USB" => "USB audio",
        "CRD" => "Card / WLIVE",
        "MOD" => "Expansion module",
        "REC" => "Recorder",
        "AES" => "AES/EBU",
        other => other,
    }
    .to_string()
}
