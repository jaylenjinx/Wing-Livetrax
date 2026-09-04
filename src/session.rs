//! Create a LiveTrax session whose tracks are named after the WING channels.
//!
//! LiveTrax sessions are Ardour-format XML. Synthesising a route graph from
//! nothing is version-sensitive and easy to get subtly wrong, so the generator
//! instead clones a real track out of a **template session** produced by your
//! own LiveTrax build, renames it, and gives every cloned node fresh ids.
//! Whatever your version writes for a track is what ends up in the new session.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use xmltree::{Element, EmitterConfig, XMLNode};

#[derive(Debug, Clone)]
pub struct SessionRequest {
    /// Folder that will contain the new session folder.
    pub parent_dir: PathBuf,
    /// Session name; also the session folder and `.ardour` file name.
    pub name: String,
    pub sample_rate: u32,
    /// Track names, in order. Usually the WING channel names.
    pub tracks: Vec<String>,
    /// Session or `.template` file to clone a track from. When absent, one is
    /// discovered from the LiveTrax/Mixbus/Ardour template folders.
    pub template: Option<PathBuf>,
    /// Point track N's input at `system:capture_N`.
    pub connect_inputs: bool,
    /// Allow the synthesised fallback when no template can be found.
    pub allow_minimal: bool,
}

#[derive(Debug, Clone)]
pub struct SessionReport {
    pub folder: PathBuf,
    pub session_file: PathBuf,
    pub tracks: usize,
    pub template: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// Clean up track names: strip characters that break port names, drop blanks,
/// and make duplicates unique - LiveTrax needs distinct route names.
pub fn normalise_track_names(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in raw {
        let cleaned: String = name.trim().replace(['/', '\\', ':'], "-");
        let base = if cleaned.is_empty() { "Track".to_string() } else { cleaned };
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

/// Write a new session. Never touches an existing folder.
pub fn create(req: &SessionRequest) -> Result<SessionReport> {
    let name = req.name.trim();
    if name.is_empty() {
        bail!("session name is empty");
    }
    if name.contains(['/', '\\', ':']) {
        bail!("session name must not contain path separators");
    }
    if req.tracks.is_empty() {
        bail!("no tracks selected");
    }
    let req = &SessionRequest {
        tracks: normalise_track_names(req.tracks.clone()),
        ..req.clone()
    };
    if !req.parent_dir.is_dir() {
        bail!("{} is not a folder", req.parent_dir.display());
    }
    let folder = req.parent_dir.join(name);
    if folder.exists() {
        bail!("{} already exists - pick another name", folder.display());
    }

    let template = match &req.template {
        Some(p) => Some(p.clone()),
        None => discover_templates().into_iter().next(),
    };

    let mut warnings = Vec::new();
    let root = match &template {
        Some(path) => build_from_template(path, req, &mut warnings)
            .with_context(|| format!("building session from template {}", path.display()))?,
        None => {
            if !req.allow_minimal {
                bail!(
                    "no template session found. Save one empty session (with at least one \
                     audio track) from LiveTrax and point the Template field at its .ardour \
                     file, or tick \"allow minimal\" to write a best-effort session."
                );
            }
            warnings.push(
                "Written without a template: the route graph is synthesised and your LiveTrax \
                 build may refuse to open it. Prefer a template session."
                    .into(),
            );
            build_minimal(req)
        }
    };

    // Everything below creates files; unwind the folder if any step fails.
    let session_file = folder.join(format!("{name}.ardour"));
    let guard = FolderGuard::new(&folder);
    for sub in [
        PathBuf::from("analysis"),
        PathBuf::from("dead"),
        PathBuf::from("export"),
        PathBuf::from("externals"),
        PathBuf::from("peaks"),
        PathBuf::from("plugins"),
        Path::new("interchange").join(name).join("audiofiles"),
        Path::new("interchange").join(name).join("midifiles"),
    ] {
        std::fs::create_dir_all(folder.join(sub)).context("creating session folders")?;
    }

    let file = std::fs::File::create(&session_file)
        .with_context(|| format!("creating {}", session_file.display()))?;
    root.write_with_config(
        std::io::BufWriter::new(file),
        EmitterConfig::new().perform_indent(true),
    )
    .context("writing session XML")?;
    guard.keep();

    Ok(SessionReport {
        folder,
        session_file,
        tracks: req.tracks.len(),
        template,
        warnings,
    })
}

/// Removes a partially written session folder unless `keep` is called.
struct FolderGuard {
    path: PathBuf,
    keep: std::cell::Cell<bool>,
}

impl FolderGuard {
    fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), keep: std::cell::Cell::new(false) }
    }
    fn keep(&self) {
        self.keep.set(true);
    }
}

impl Drop for FolderGuard {
    fn drop(&mut self) {
        if !self.keep.get() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

// -------------------------------------------------------------- template ---

fn build_from_template(
    path: &Path,
    req: &SessionRequest,
    warnings: &mut Vec<String>,
) -> Result<Element> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut root = Element::parse(std::io::BufReader::new(file)).context("parsing template XML")?;
    if root.name != "Session" {
        bail!("{} is not a session file (root element is <{}>)", path.display(), root.name);
    }

    root.attributes.insert("name".into(), req.name.clone());
    root.attributes.insert("sample-rate".into(), req.sample_rate.to_string());

    let mut next_id = max_id(&root) + 1;

    let routes = root
        .get_mut_child("Routes")
        .context("template session has no <Routes> section")?;

    let proto = routes
        .children
        .iter()
        .filter_map(XMLNode::as_element)
        .find(|e| e.name == "Route" && is_audio_track(e))
        .cloned()
        .context(
            "template session has no audio track to copy - save a template that contains at \
             least one audio track",
        )?;

    // Keep the master/monitor busses, drop the template's own tracks.
    let dropped: Vec<String> = routes
        .children
        .iter()
        .filter_map(XMLNode::as_element)
        .filter(|e| e.name == "Route" && is_track(e))
        .filter_map(|e| e.attributes.get("name").cloned())
        .collect();
    let mut kept: Vec<XMLNode> = routes
        .children
        .iter()
        .filter(|n| match n.as_element() {
            Some(e) if e.name == "Route" => !is_track(e),
            _ => true,
        })
        .cloned()
        .collect();
    for node in kept.iter_mut() {
        if let Some(el) = node.as_mut_element() {
            drop_stale_connections(el, &dropped);
        }
    }

    for (i, track) in req.tracks.iter().enumerate() {
        kept.push(XMLNode::Element(clone_route(
            &proto,
            track,
            i,
            &mut next_id,
            req.connect_inputs,
        )));
    }
    routes.children = kept;

    // Anything that referred to the template's tracks or media has to go.
    for section in ["Playlists", "UnusedPlaylists", "Regions", "Sources", "Selection"] {
        if let Some(el) = root.get_mut_child(section) {
            el.children.clear();
        }
    }
    if let Some(locations) = root.get_mut_child("Locations") {
        locations.children.retain(|n| {
            n.as_element()
                .and_then(|e| e.attributes.get("flags"))
                .map(|f| f.contains("IsSessionRange"))
                .unwrap_or(false)
        });
    }
    if let Some(groups) = root.get_mut_child("RouteGroups") {
        for node in groups.children.iter_mut() {
            if let Some(el) = node.as_mut_element() {
                el.attributes.insert("routes".into(), String::new());
            }
        }
    }

    root.attributes.insert("id-counter".into(), (next_id + 1).to_string());
    if req.connect_inputs {
        warnings.push(
            "Track inputs were pointed at system:capture_1..N. Check them against your \
             interface in LiveTrax."
                .into(),
        );
    }
    Ok(root)
}

/// A route that is a track (has a diskstream/playlist) rather than a bus.
pub fn is_track(e: &Element) -> bool {
    if let Some(flags) = presentation_flags(e) {
        if flags.contains("MasterOut") || flags.contains("MonitorOut") || flags.contains("Auditioner")
        {
            return false;
        }
        return flags.contains("Track");
    }
    // Older layouts: anything referencing a playlist is a track.
    ["audio-playlist", "midi-playlist", "playlist", "diskstream-id"]
        .iter()
        .any(|k| e.attributes.contains_key(*k))
}

fn is_audio_track(e: &Element) -> bool {
    if !is_track(e) {
        return false;
    }
    match e.attributes.get("default-type") {
        Some(t) => t == "audio",
        None => presentation_flags(e).map(|f| f.contains("AudioTrack")).unwrap_or(true),
    }
}

pub fn presentation_flags(e: &Element) -> Option<String> {
    if let Some(pi) = e.get_child("PresentationInfo") {
        if let Some(f) = pi.attributes.get("flags") {
            return Some(f.clone());
        }
    }
    e.attributes.get("flags").cloned()
}

fn clone_route(
    proto: &Element,
    new_name: &str,
    order: usize,
    next_id: &mut u64,
    connect_inputs: bool,
) -> Element {
    let mut route = proto.clone();
    let old_name = route.attributes.get("name").cloned().unwrap_or_default();

    route.attributes.insert("name".into(), new_name.to_string());
    // Let LiveTrax create fresh, empty playlists for the new track.
    for key in ["audio-playlist", "midi-playlist", "playlist", "diskstream-id"] {
        route.attributes.shift_remove(key);
    }
    if let Some(pi) = route.get_mut_child("PresentationInfo") {
        pi.attributes.insert("order".into(), order.to_string());
    }

    rename_in_tree(&mut route, &old_name, new_name);
    reassign_ids(&mut route, next_id);
    if connect_inputs {
        connect_capture(&mut route, order + 1);
    }
    route
}

/// Rewrite the route name wherever it is embedded: IO names, port names.
fn rename_in_tree(el: &mut Element, old: &str, new: &str) {
    if let Some(name) = el.attributes.get("name").cloned() {
        if name == old {
            el.attributes.insert("name".into(), new.to_string());
        } else if let Some(rest) = name.strip_prefix(&format!("{old}/")) {
            el.attributes.insert("name".into(), format!("{new}/{rest}"));
        }
    }
    for child in el.children.iter_mut() {
        if let Some(child) = child.as_mut_element() {
            rename_in_tree(child, old, new);
        }
    }
}

/// Every numeric `id` in the clone must be unique across the session.
fn reassign_ids(el: &mut Element, next_id: &mut u64) {
    if let Some(id) = el.attributes.get("id") {
        if id.parse::<u64>().is_ok() {
            el.attributes.insert("id".into(), next_id.to_string());
            *next_id += 1;
        }
    }
    for child in el.children.iter_mut() {
        if let Some(child) = child.as_mut_element() {
            reassign_ids(child, next_id);
        }
    }
}

/// Point the track's inputs at `system:capture_<n>`, replacing whatever the
/// template was connected to (ports carry several <Connection> children).
fn connect_capture(route: &mut Element, n: usize) {
    for node in route.children.iter_mut() {
        let Some(io) = node.as_mut_element() else { continue };
        if io.name != "IO" || io.attributes.get("direction").map(String::as_str) != Some("Input") {
            continue;
        }
        let mut port_index = 0usize;
        for pnode in io.children.iter_mut() {
            let Some(port) = pnode.as_mut_element() else { continue };
            if port.name != "Port" {
                continue;
            }
            port_index += 1;
            let mut conn = Element::new("Connection");
            conn.attributes
                .insert("other".into(), format!("system:capture_{}", n + port_index - 1));
            port.children
                .retain(|c| c.as_element().map(|e| e.name != "Connection").unwrap_or(true));
            port.children.push(XMLNode::Element(conn));
        }
    }
}

/// Drop connections that point at ports of routes we did not carry over -
/// the master bus in a real session lists every old track by name.
fn drop_stale_connections(el: &mut Element, gone: &[String]) {
    if el.name == "Port" {
        el.children.retain(|node| {
            let Some(conn) = node.as_element() else { return true };
            if conn.name != "Connection" {
                return true;
            }
            let Some(other) = conn.attributes.get("other") else { return true };
            !gone.iter().any(|name| other.starts_with(&format!("{name}/")))
        });
    }
    for child in el.children.iter_mut() {
        if let Some(child) = child.as_mut_element() {
            drop_stale_connections(child, gone);
        }
    }
}

fn max_id(el: &Element) -> u64 {
    let mut best = el
        .attributes
        .get("id")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .max(
            el.attributes
                .get("id-counter")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0),
        );
    for child in el.children.iter() {
        if let Some(child) = child.as_element() {
            best = best.max(max_id(child));
        }
    }
    best
}

// --------------------------------------------------------------- minimal ---

/// Best-effort session with no template to copy from.
fn build_minimal(req: &SessionRequest) -> Element {
    let mut root = Element::new("Session");
    root.attributes.insert("version".into(), "7000".into());
    root.attributes.insert("name".into(), req.name.clone());
    root.attributes.insert("sample-rate".into(), req.sample_rate.to_string());
    root.attributes.insert("end-is-free".into(), "1".into());
    root.attributes.insert("session-range-is-free".into(), "1".into());

    let mut id = 100u64;
    let mut next = || {
        id += 1;
        id.to_string()
    };

    for section in ["Config", "Metadata", "Sources", "Regions", "Playlists", "UnusedPlaylists", "RouteGroups", "Click", "Speakers", "TempoMap", "Bundles", "VCAManager", "Extra"] {
        root.children.push(XMLNode::Element(Element::new(section)));
    }

    let mut locations = Element::new("Locations");
    let mut range = Element::new("Location");
    range.attributes.insert("id".into(), next());
    range.attributes.insert("name".into(), "session".into());
    range.attributes.insert("start".into(), "0".into());
    range.attributes.insert("end".into(), req.sample_rate.to_string());
    range.attributes.insert("flags".into(), "IsSessionRange".into());
    range.attributes.insert("locked".into(), "no".into());
    locations.children.push(XMLNode::Element(range));
    root.children.push(XMLNode::Element(locations));

    let mut routes = Element::new("Routes");
    for (i, name) in req.tracks.iter().enumerate() {
        let mut route = Element::new("Route");
        route.attributes.insert("id".into(), next());
        route.attributes.insert("name".into(), name.clone());
        route.attributes.insert("default-type".into(), "audio".into());
        route.attributes.insert("strict-io".into(), "1".into());
        route.attributes.insert("active".into(), "1".into());
        route.attributes.insert("denormal-protection".into(), "0".into());
        route.attributes.insert("meter-point".into(), "MeterPostFader".into());
        route.attributes.insert("disk-io-point".into(), "DiskIOPreFader".into());
        route.attributes.insert("mode".into(), "Normal".into());

        let mut pi = Element::new("PresentationInfo");
        pi.attributes.insert("order".into(), i.to_string());
        pi.attributes.insert("flags".into(), "AudioTrack".into());
        route.children.push(XMLNode::Element(pi));

        for (direction, count) in [("Input", 1usize), ("Output", 2usize)] {
            let mut io = Element::new("IO");
            io.attributes.insert("id".into(), next());
            io.attributes.insert("name".into(), name.clone());
            io.attributes.insert("direction".into(), direction.into());
            io.attributes.insert("default-type".into(), "audio".into());
            io.attributes.insert("user-latency".into(), "0".into());
            for p in 1..=count {
                let mut port = Element::new("Port");
                port.attributes.insert("type".into(), "audio".into());
                let suffix = if direction == "Input" { "audio_in" } else { "audio_out" };
                port.attributes.insert("name".into(), format!("{name}/{suffix} {p}"));
                if direction == "Input" && req.connect_inputs {
                    let mut conn = Element::new("Connection");
                    conn.attributes
                        .insert("other".into(), format!("system:capture_{}", i + 1));
                    port.children.push(XMLNode::Element(conn));
                }
                io.children.push(XMLNode::Element(port));
            }
            route.children.push(XMLNode::Element(io));
        }

        let mut amp = Element::new("Processor");
        amp.attributes.insert("id".into(), next());
        amp.attributes.insert("name".into(), "Amp".into());
        amp.attributes.insert("active".into(), "1".into());
        amp.attributes.insert("type".into(), "amp".into());
        let mut gain = Element::new("Controllable");
        gain.attributes.insert("name".into(), "gaincontrol".into());
        gain.attributes.insert("id".into(), next());
        gain.attributes.insert("value".into(), "1".into());
        amp.children.push(XMLNode::Element(gain));
        route.children.push(XMLNode::Element(amp));

        routes.children.push(XMLNode::Element(route));
    }
    root.children.push(XMLNode::Element(routes));
    root.attributes.insert("id-counter".into(), (id + 1).to_string());
    root
}

// ------------------------------------------------------------- discovery ---

/// Session templates shipped or saved by LiveTrax, Mixbus or Ardour.
pub fn discover_templates() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let roots = [
        home.join("Library/Preferences"),
        home.join(".config"),
    ];
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_lowercase();
            if !["livetrax", "mixbus", "ardour"].iter().any(|p| dir_name.starts_with(p)) {
                continue;
            }
            let templates = entry.path().join("templates");
            let Ok(kids) = std::fs::read_dir(&templates) else { continue };
            for kid in kids.flatten() {
                // Templates are either <name>/<name>.template or <name>.template.
                let path = kid.path();
                let candidates: Vec<PathBuf> = if path.is_dir() {
                    std::fs::read_dir(&path)
                        .map(|d| d.flatten().map(|e| e.path()).collect())
                        .unwrap_or_default()
                } else {
                    vec![path]
                };
                for c in candidates {
                    let ext = c.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if (ext == "template" || ext == "ardour") && seen.insert(c.clone()) {
                        found.push(c);
                    }
                }
            }
        }
    }
    found.sort();
    found
}
