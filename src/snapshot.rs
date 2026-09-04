//! Offline WING snapshot from a LiveTrax session.
//!
//! Reads track names straight out of a session file - no DAW and no console
//! needed - and turns them into channel names for the WING.
//!
//! Behringer's snapshot container is not a documented format, so this writes
//! **node text**: one `<address> <value>` line per channel, in the same address
//! space the console speaks over OSC. That file is readable, editable, and can
//! be pushed to the console by this tool. If you have a snapshot exported from
//! your own WING and it is a text file, pass it as a template and only the name
//! entries are rewritten, leaving the rest of the file untouched.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use xmltree::{Element, XMLNode};

use crate::config;
use crate::osc;
use crate::session;
use crate::snapfile::SnapFile;

#[derive(Debug, Clone)]
pub struct SnapshotRequest {
    /// Session file, or the folder containing it.
    pub session: PathBuf,
    /// Console channel that the first track maps to.
    pub first_channel: u16,
    /// Truncate names to this many characters. 0 = no limit.
    pub max_len: usize,
    /// Include busses as well as tracks.
    pub include_busses: bool,
    /// Snapshot exported from the console, to be rewritten in place. A real
    /// WING `.snap` (JSON) is edited properly; a text file is rewritten line by
    /// line; a binary one is refused.
    pub template: Option<PathBuf>,
    /// Console output group the DAW records. With a `.snap` template, track N
    /// is named onto the channel feeding output N of this group instead of
    /// onto channel N.
    pub output_group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Console channel this name is written to.
    pub channel: u16,
    /// Output of the selected group that led to that channel, if a patch was
    /// used to work it out.
    pub output: Option<u16>,
    /// Name as it appears in the session.
    pub track: String,
    /// Name as it will be written to the console.
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct SnapshotPlan {
    pub session_name: String,
    pub entries: Vec<Entry>,
    /// Tracks whose output carries no console channel, so there is nothing to
    /// name: (output, track name).
    pub skipped: Vec<(u16, String)>,
}

#[derive(Debug, Clone)]
pub struct WriteReport {
    pub path: PathBuf,
    pub written: usize,
    /// Channels the template had no line for.
    pub unmatched: Vec<u16>,
    pub template: Option<PathBuf>,
}

/// Track (and optionally bus) names from a session, in presentation order.
pub fn read_tracks(path: &Path, include_busses: bool) -> Result<Vec<String>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let root = Element::parse(std::io::BufReader::new(file))
        .with_context(|| format!("parsing {}", path.display()))?;
    if root.name != "Session" {
        bail!("{} is not a session file", path.display());
    }
    let routes = root
        .get_child("Routes")
        .context("session has no <Routes> section")?;

    let mut found: Vec<(i64, String)> = Vec::new();
    for (i, el) in routes
        .children
        .iter()
        .filter_map(XMLNode::as_element)
        .filter(|e| e.name == "Route")
        .enumerate()
    {
        let flags = session::presentation_flags(el).unwrap_or_default();
        if flags.contains("MasterOut") || flags.contains("MonitorOut") || flags.contains("Auditioner")
        {
            continue;
        }
        if !include_busses && !session::is_track(el) {
            continue;
        }
        let Some(name) = el.attributes.get("name").cloned() else { continue };
        let order = el
            .get_child("PresentationInfo")
            .and_then(|pi| pi.attributes.get("order"))
            .and_then(|o| o.parse::<i64>().ok())
            .unwrap_or(i as i64);
        found.push((order, name));
    }
    found.sort_by_key(|(order, _)| *order);
    Ok(found.into_iter().map(|(_, name)| name).collect())
}

/// Work out which console channel gets which name.
pub fn plan(req: &SnapshotRequest) -> Result<SnapshotPlan> {
    let file = crate::markers::resolve_session_path(&req.session)?;
    let tracks = read_tracks(&file, req.include_busses)?;
    let session_name = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    // With a .snap template and a chosen output group, names follow the
    // console's own patching rather than raw channel order.
    let (entries, skipped) = match (&req.template, &req.output_group) {
        (Some(template), Some(group)) if is_snap(template) => {
            let snap = SnapFile::load(template)?;
            map_entries_via_patch(tracks, &snap, group, req.first_channel, req.max_len)
        }
        _ => (map_entries(tracks, req.first_channel, req.max_len), Vec::new()),
    };
    Ok(SnapshotPlan { session_name, entries, skipped })
}

/// Lay track names out across console channels. Pure, so the GUI can re-run it
/// as the channel offset or length limit is dragged.
pub fn map_entries(tracks: Vec<String>, first_channel: u16, max_len: usize) -> Vec<Entry> {
    tracks
        .into_iter()
        .enumerate()
        .map(|(i, track)| {
            let channel = first_channel.saturating_add(i as u16);
            let name = if max_len > 0 && track.chars().count() > max_len {
                track.chars().take(max_len).collect()
            } else {
                track.clone()
            };
            Entry { channel, output: None, track, name }
        })
        .collect()
}

/// Lay track names out across the channels that feed a group's outputs, so
/// track N names whatever the console actually sends on output N.
pub fn map_entries_via_patch(
    tracks: Vec<String>,
    snap: &SnapFile,
    group: &str,
    first_output: u16,
    max_len: usize,
) -> (Vec<Entry>, Vec<(u16, String)>) {
    let slots = snap.outputs(group);
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    for (i, track) in tracks.into_iter().enumerate() {
        let output = first_output.saturating_add(i as u16);
        let channel = slots
            .iter()
            .find(|s| s.output == output)
            .and_then(|s| s.channel);
        match channel {
            Some(channel) => {
                let name = if max_len > 0 && track.chars().count() > max_len {
                    track.chars().take(max_len).collect()
                } else {
                    track.clone()
                };
                entries.push(Entry { channel, output: Some(output), track, name });
            }
            // Nothing on that output, or it carries a bus rather than a
            // channel: there is no channel name to set.
            None => skipped.push((output, track)),
        }
    }
    (entries, skipped)
}

/// Write the snapshot, rewriting a console export when one was given.
pub fn write(
    plan: &SnapshotPlan,
    req: &SnapshotRequest,
    cfg: &config::Snapshot,
    wing: &config::Wing,
    out: &Path,
) -> Result<WriteReport> {
    match &req.template {
        Some(template) if is_snap(template) => write_snap(plan, template, out),
        Some(template) => rewrite_template(plan, cfg, wing, template, out),
        None => write_node_text(plan, cfg, wing, out),
    }
}

/// A WING `.snap` is JSON; anything else gets the text or node-text path.
fn is_snap(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("snap"))
        == Some(true)
}

/// Rename channels inside a real console snapshot and save it as a new one.
/// Every other setting in the file is left exactly as the console wrote it.
fn write_snap(plan: &SnapshotPlan, template: &Path, out: &Path) -> Result<WriteReport> {
    let mut snap = SnapFile::load(template)?;
    let mut written = 0;
    let mut unmatched = Vec::new();
    for entry in &plan.entries {
        match snap.set_channel_name(entry.channel, &entry.name) {
            Ok(()) => written += 1,
            Err(_) => unmatched.push(entry.channel),
        }
    }
    snap.save(out)?;
    Ok(WriteReport {
        path: out.to_path_buf(),
        written,
        unmatched,
        template: Some(template.to_path_buf()),
    })
}

/// The address a channel's name lives at, from the same template the live
/// bridge uses.
fn name_address(wing: &config::Wing, channel: u16) -> String {
    osc::template(&wing.name_address, &[("ch", channel.to_string())])
}

fn quote(name: &str) -> String {
    if name.is_empty() || name.contains(char::is_whitespace) {
        format!("\"{name}\"")
    } else {
        name.to_string()
    }
}

fn write_node_text(
    plan: &SnapshotPlan,
    cfg: &config::Snapshot,
    wing: &config::Wing,
    out: &Path,
) -> Result<WriteReport> {
    let mut text = String::new();
    if cfg.include_comments {
        text.push_str(&format!(
            "# WING channel names from LiveTrax session \"{}\"\n\
             # {} channels, written by wing-livetrax-bridge {}\n",
            plan.session_name,
            plan.entries.len(),
            env!("CARGO_PKG_VERSION")
        ));
    }
    for entry in &plan.entries {
        let line = osc::template(
            &cfg.line,
            &[
                ("path", name_address(wing, entry.channel)),
                ("ch", entry.channel.to_string()),
                ("name", quote(&entry.name)),
                ("raw", entry.name.clone()),
            ],
        );
        text.push_str(&line);
        text.push('\n');
    }
    std::fs::write(out, text).with_context(|| format!("writing {}", out.display()))?;
    Ok(WriteReport {
        path: out.to_path_buf(),
        written: plan.entries.len(),
        unmatched: Vec::new(),
        template: None,
    })
}

/// Rewrite only the name lines of a console export, leaving everything else
/// byte-for-byte as it was.
fn rewrite_template(
    plan: &SnapshotPlan,
    cfg: &config::Snapshot,
    wing: &config::Wing,
    template: &Path,
    out: &Path,
) -> Result<WriteReport> {
    let bytes = std::fs::read(template)
        .with_context(|| format!("reading {}", template.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "{} is not a text file - this tool will not rewrite a binary snapshot. \
             Write a node-text file instead (leave the template blank).",
            template.display()
        )
    })?;

    // Every address form a channel's name might appear under.
    let mut wanted: HashMap<String, &Entry> = HashMap::new();
    for entry in &plan.entries {
        for pattern in cfg.name_paths.iter().chain(std::iter::once(&wing.name_address)) {
            let path = osc::template(pattern, &[("ch", entry.channel.to_string())]);
            wanted.insert(path.trim().to_lowercase(), entry);
        }
    }

    let mut matched: Vec<u16> = Vec::new();
    let mut result = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let token = body.split_whitespace().next().unwrap_or("");
        match wanted.get(&token.to_lowercase()) {
            Some(entry) => {
                let indent = &body[..body.len() - body.trim_start().len()];
                result.push_str(&format!("{indent}{token} {}{ending}", quote(&entry.name)));
                matched.push(entry.channel);
            }
            None => result.push_str(line),
        }
    }

    let unmatched: Vec<u16> = plan
        .entries
        .iter()
        .map(|e| e.channel)
        .filter(|ch| !matched.contains(ch))
        .collect();
    std::fs::write(out, result).with_context(|| format!("writing {}", out.display()))?;
    Ok(WriteReport {
        path: out.to_path_buf(),
        written: matched.len(),
        unmatched,
        template: Some(template.to_path_buf()),
    })
}
