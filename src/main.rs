//! wing-livetrax-bridge - links a Behringer WING to Harrison LiveTrax 3.

mod bridge;
mod config;
mod gui;
mod livetrax;
mod markers;
mod osc;
mod patchbuild;
mod session;
mod patch;
mod sheet;
mod prefs;
mod quscene;
mod shared;
mod snapfile;
mod snapshot;
mod theme;
mod timecode;
mod wing;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rosc::OscType;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::livetrax::Daw;
use crate::osc::{Incoming, OscLink};
use crate::session::SessionRequest;
use crate::snapshot::SnapshotRequest;
use crate::shared::{command_channel, LogBuffer};
use crate::wing::Wing;

#[derive(Parser, Debug)]
#[command(name = "wing-livetrax-bridge", version, about, long_about = None)]
struct Cli {
    /// Path to the TOML configuration. Defaults to ./config.toml when there is
    /// one, otherwise the per-user copy in Application Support.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// Log every OSC message in both directions.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Defaults to the graphical front end when omitted.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the bridge with the graphical front end (the default).
    Gui {
        /// Open on a specific tab: channels, transport, scenes, new-session,
        /// snapshot, log - or "preferences", optionally with a section, as in
        /// "preferences/transport".
        #[arg(long)]
        tab: Option<String>,
        /// Place the window at "x,y" instead of letting the OS choose.
        #[arg(long, hide = true)]
        position: Option<String>,
    },
    /// Run the bridge headless, logging to the terminal.
    Run,
    /// Create a LiveTrax session whose tracks are named after the WING channels.
    NewSession {
        /// Folder that will contain the new session folder.
        #[arg(long)]
        dest: PathBuf,
        /// Session name.
        #[arg(long)]
        name: String,
        /// Channel range to take names from - output range when a patch is
        /// used, e.g. 1-16.
        #[arg(long, default_value = "1-16", alias = "outputs")]
        channels: String,
        /// Take names from a WING .snap's output patch instead of asking the
        /// console. Defaults to patch.snap_file when one is configured.
        #[arg(long)]
        from_snap: Option<PathBuf>,
        /// Take names from an Allen & Heath Qu scene (.DAT) saved to USB.
        #[arg(long)]
        from_scene: Option<PathBuf>,
        /// With --from-scene, add the stereo inputs and FX returns after the
        /// mono inputs.
        #[arg(long)]
        include_extras: bool,
        /// Output group to read from that snapshot. Defaults to
        /// patch.output_group.
        #[arg(long)]
        output: Option<String>,
        /// Ignore any snapshot and read names from the live console.
        #[arg(long)]
        no_patch: bool,
        /// Session or .template file to clone a track from.
        #[arg(long)]
        template: Option<PathBuf>,
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        /// Include channels with no name, as "Ch N".
        #[arg(long)]
        include_unnamed: bool,
        /// Do not point track inputs at system:capture_N.
        #[arg(long)]
        no_connect_inputs: bool,
        /// Write a synthesised session when no template is available.
        #[arg(long)]
        allow_minimal: bool,
        /// How long to collect names from the console first.
        #[arg(long, default_value_t = 2)]
        wait_secs: u64,
    },
    /// Print every OSC message received, without changing anything.
    Probe {
        #[arg(long, value_enum, default_value_t = Target::Both)]
        target: Target,
        /// Stop after this many seconds. 0 = run until Ctrl-C.
        #[arg(long, default_value_t = 0)]
        seconds: u64,
        /// Only show addresses containing this substring.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Watch the console and print ready-to-paste config for what you touch.
    Learn {
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Ask LiveTrax for its strip list and print it.
    Strips {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Write WING channel names from a LiveTrax session, without either device.
    WingSnapshot {
        /// Session file or folder. Defaults to livetrax.session_file.
        #[arg(long)]
        session: Option<PathBuf>,
        /// File to write. Omit to preview only.
        #[arg(long)]
        out: Option<PathBuf>,
        /// WING .snap to rewrite. Channel names are replaced inside it and the
        /// result is a loadable snapshot. Defaults to patch.snap_file.
        #[arg(long)]
        template: Option<PathBuf>,
        /// Console output group the DAW records, e.g. USB. Track N then names
        /// the channel feeding output N. Defaults to patch.output_group.
        #[arg(long)]
        output: Option<String>,
        /// Console channel - or, with --output, console output - that the
        /// first track maps to.
        #[arg(long, alias = "first-output")]
        first_channel: Option<u16>,
        /// Truncate names to this length. 0 = no limit.
        #[arg(long)]
        max_len: Option<usize>,
        /// Include busses as well as tracks.
        #[arg(long)]
        include_busses: bool,
        /// Also push the names to the console over OSC.
        #[arg(long)]
        apply: bool,
    },
    /// Read the output patch from the live console and print it.
    Patch {
        /// Output group the DAW records, e.g. USB. Defaults to
        /// patch.output_group.
        #[arg(long)]
        output: Option<String>,
        /// How long to collect replies.
        #[arg(long, default_value_t = 4)]
        seconds: u64,
        /// List every group the console answered for, not one group's names.
        #[arg(long)]
        groups: bool,
    },
    /// Inspect an Allen & Heath Qu scene (.DAT) saved to USB.
    QuScene {
        /// The scene file, e.g. Scene001.DAT.
        file: PathBuf,
        /// Show every slot in the file, not just the inputs that are named.
        #[arg(long)]
        all: bool,
    },
    /// Inspect a WING .snap file: channel names and output patches.
    SnapInfo {
        /// The .snap file.
        file: PathBuf,
        /// Show the resolved names for one output group, e.g. USB.
        #[arg(long)]
        output: Option<String>,
    },
    /// Print the markers parsed from the configured session file.
    Markers,
    /// Send one OSC message by hand. Args: `i:1`, `f:0.5`, `s:text`, or bare.
    Send {
        #[arg(long, value_enum)]
        target: Target,
        address: String,
        args: Vec<String>,
    },
    /// Write a starter configuration file.
    Init {
        #[arg(short, long, default_value = "config.toml")]
        output: PathBuf,
    },
    /// Build a console snapshot and a LiveTrax session from a patch sheet.
    ///
    /// The sheet is a CSV exported from whatever the patch was planned in.
    /// One row per channel: name, the socket it arrives on, gain, phantom,
    /// colour, DCA, and which track records it. Write a starter sheet with
    /// `patch-template`.
    Build {
        /// The patch sheet (.csv, .tsv).
        #[arg(long)]
        sheet: PathBuf,
        /// Desk the sheet is for: wing, qu-16, qu-24, qu-32. The Qu desks have
        /// no published snapshot format, so for those the sheet builds the
        /// LiveTrax session and nothing else.
        #[arg(long, default_value = "wing", alias = "console")]
        desk: String,
        /// Folder that will contain the new session folder. Omit to write only
        /// the snapshot.
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Show name: the session folder, and the snapshot's file name.
        #[arg(long)]
        name: String,
        /// Where to write the .snap. Defaults to beside the session, or the
        /// working directory when no session is being made.
        #[arg(long)]
        snap: Option<PathBuf>,
        /// Snapshot to overlay the sheet onto. Defaults to a factory console,
        /// so pass your own show file to keep its effects and bus structure.
        #[arg(long)]
        base: Option<PathBuf>,
        /// Port group the DAW records from. Defaults to patch.output_group.
        #[arg(long)]
        output: Option<String>,
        /// Session or .template file to clone tracks from.
        #[arg(long)]
        template: Option<PathBuf>,
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        /// Do not point track inputs at system:capture_N.
        #[arg(long)]
        no_connect_inputs: bool,
        /// Do not arm the created tracks for record.
        #[arg(long)]
        no_arm: bool,
        /// Write a synthesised session when no template is available.
        #[arg(long)]
        allow_minimal: bool,
        /// Leave the record group's other outputs as the base had them.
        #[arg(long)]
        keep_unlisted_outputs: bool,
        /// Do not copy each channel's name, colour and icon onto the socket
        /// it takes. They are kept together because the console can be set to
        /// show either one on the scribble strip.
        #[arg(long)]
        no_source_labels: bool,
        /// Print every node the sheet would move, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write a starter patch sheet, with the column reference and an example.
    PatchTemplate {
        #[arg(short, long, default_value = "patch-sheet.csv")]
        output: PathBuf,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Target {
    Wing,
    Daw,
    Both,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let default_level = if cli.verbose {
        "wing_livetrax_bridge=trace"
    } else {
        "wing_livetrax_bridge=info"
    };
    // The GUI reads the same log stream the terminal gets.
    let log = LogBuffer::default();
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_level.into()),
        )
        .with_target(false)
        .with_ansi(false)
        .with_writer(log.clone().and(std::io::stdout))
        .init();

    let config_path = match resolve_config(cli.config) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    };

    match cli.cmd.unwrap_or(Cmd::Gui { tab: None, position: None }) {
        Cmd::Init { output } => cmd_init(&output),
        Cmd::Markers => cmd_markers(&config_path),
        Cmd::SnapInfo { file, output } => cmd_snap_info(&file, output.as_deref()),
        Cmd::QuScene { file, all } => cmd_qu_scene(&file, all),
        Cmd::Patch { output, seconds, groups } => {
            block_on(cmd_patch(&config_path, output, seconds, groups))
        }
        Cmd::Gui { tab, position } => cmd_gui(&config_path, log, tab.as_deref(), position.as_deref()),
        Cmd::Run => block_on(cmd_run(&config_path)),
        Cmd::Probe { target, seconds, filter } => {
            block_on(cmd_probe(&config_path, target, seconds, filter))
        }
        Cmd::Learn { seconds } => block_on(cmd_learn(&config_path, seconds)),
        Cmd::Strips { seconds } => block_on(cmd_strips(&config_path, seconds)),
        Cmd::Send { target, address, args } => {
            block_on(cmd_send(&config_path, target, &address, &args))
        }
        Cmd::WingSnapshot {
            session,
            out,
            template,
            output,
            first_channel,
            max_len,
            include_busses,
            apply,
        } => block_on(cmd_wing_snapshot(
            &config_path,
            WingSnapshotArgs {
                session,
                out,
                template,
                output,
                first_channel,
                max_len,
                include_busses,
                apply,
            },
        )),
        Cmd::NewSession {
            dest,
            name,
            channels,
            from_snap,
            from_scene,
            include_extras,
            output,
            no_patch,
            template,
            rate,
            include_unnamed,
            no_connect_inputs,
            allow_minimal,
            wait_secs,
        } => block_on(cmd_new_session(
            &config_path,
            NewSessionArgs {
                dest,
                name,
                channels,
                from_snap,
                from_scene,
                include_extras,
                output,
                no_patch,
                template,
                rate,
                include_unnamed,
                connect_inputs: !no_connect_inputs,
                allow_minimal,
                wait_secs,
            },
        )),
        Cmd::PatchTemplate { output } => cmd_patch_template(&output),
        Cmd::Build {
            sheet,
            desk,
            dest,
            name,
            snap,
            base,
            output,
            template,
            rate,
            no_connect_inputs,
            no_arm,
            allow_minimal,
            keep_unlisted_outputs,
            no_source_labels,
            dry_run,
        } => cmd_build(
            &config_path,
            BuildArgs {
                sheet,
                desk,
                dest,
                name,
                snap,
                base,
                output,
                template,
                rate,
                connect_inputs: !no_connect_inputs,
                arm: !no_arm,
                allow_minimal,
                keep_unlisted_outputs,
                label_sources: !no_source_labels,
                dry_run,
            },
        ),
    }
}

/// Where the configuration lives.
///
/// A path on the command line always wins. Otherwise a `config.toml` in the
/// working directory is used when there is one, which is how the CLI is usually
/// run. Failing that it is the per-user copy, created from the bundled example
/// the first time the app is opened.
fn resolve_config(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let local = PathBuf::from("config.toml");
    if local.is_file() {
        return Ok(local);
    }
    let path = user_config_path()?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&path, include_str!("../config.example.toml"))
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!("Wrote a starter configuration to {}", path.display());
    }
    Ok(path)
}

fn user_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("no HOME in the environment")?;
    let dir = if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Application Support/WING LiveTrax Bridge")
    } else {
        PathBuf::from(home).join(".config/wing-livetrax-bridge")
    };
    Ok(dir.join("config.toml"))
}

/// Each subcommand gets its own runtime; the GUI needs the main thread.
fn block_on<F: std::future::Future<Output = Result<()>>>(fut: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting async runtime")?
        .block_on(fut)
}

// ------------------------------------------------------------------ setup ---

async fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    if let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() {
        return Ok(addr);
    }
    tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolving {host}:{port}"))?
        .next()
        .with_context(|| format!("no address for {host}:{port}"))
}

async fn open_wing(cfg: &Config) -> Result<(Wing, tokio::sync::mpsc::Receiver<Incoming>)> {
    let remote = resolve(&cfg.wing.host, cfg.wing.port).await?;
    let local = SocketAddr::from(([0, 0, 0, 0], cfg.wing.local_port));
    let (link, rx) = OscLink::bind("wing", local, remote).await?;
    Ok((Wing::new(link, cfg.wing.clone()), rx))
}

async fn open_daw(cfg: &Config) -> Result<(Daw, tokio::sync::mpsc::Receiver<Incoming>)> {
    let remote = resolve(&cfg.livetrax.host, cfg.livetrax.port).await?;
    let local = SocketAddr::from(([0, 0, 0, 0], cfg.livetrax.local_port));
    let (link, rx) = OscLink::bind("daw", local, remote).await?;
    Ok((Daw::new(link, cfg.livetrax.clone()), rx))
}

// --------------------------------------------------------------- commands ---

async fn cmd_run(path: &std::path::Path) -> Result<()> {
    let cfg = Config::load(path)?;
    let (wing, wing_rx) = open_wing(&cfg).await?;
    let (daw, daw_rx) = open_daw(&cfg).await?;
    // The sender is held so the command branch never sees a closed channel.
    let (_tx, cmd_rx) = command_channel();
    bridge::Bridge::new(cfg, wing, daw).run(wing_rx, daw_rx, cmd_rx).await
}

/// The GUI runs on the main thread; the bridge gets a runtime of its own.
fn cmd_gui(
    path: &std::path::Path,
    log: LogBuffer,
    tab: Option<&str>,
    position: Option<&str>,
) -> Result<()> {
    let cfg = Config::load(path)?;
    let state = shared::shared();
    let (tx, cmd_rx) = command_channel();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting async runtime")?;
    let bridge_cfg = cfg.clone();
    let bridge_state = state.clone();
    std::thread::Builder::new()
        .name("bridge".into())
        .spawn(move || {
            runtime.block_on(async move {
                let wing = match open_wing(&bridge_cfg).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("console link: {e:#}");
                        return;
                    }
                };
                let daw = match open_daw(&bridge_cfg).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("daw link: {e:#}");
                        return;
                    }
                };
                let bridge = bridge::Bridge::new(bridge_cfg, wing.0, daw.0).attach_ui(bridge_state);
                if let Err(e) = bridge.run(wing.1, daw.1, cmd_rx).await {
                    tracing::error!("bridge stopped: {e:#}");
                }
            });
        })
        .context("starting bridge thread")?;

    let position = match position {
        Some(raw) => {
            let (x, y) = raw.split_once(',').context("--position wants \"x,y\"")?;
            Some([x.trim().parse::<f32>()?, y.trim().parse::<f32>()?])
        }
        None => None,
    };
    gui::run(state, tx, log, cfg, path.to_path_buf(), tab, position)
}

struct NewSessionArgs {
    dest: PathBuf,
    name: String,
    channels: String,
    from_snap: Option<PathBuf>,
    from_scene: Option<PathBuf>,
    include_extras: bool,
    output: Option<String>,
    no_patch: bool,
    template: Option<PathBuf>,
    rate: u32,
    include_unnamed: bool,
    connect_inputs: bool,
    allow_minimal: bool,
    wait_secs: u64,
}

async fn cmd_new_session(path: &std::path::Path, args: NewSessionArgs) -> Result<()> {
    let cfg = Config::load(path)?;
    let (first, last) = parse_range(&args.channels)?;

    // Names come from a Qu scene, a saved WING output patch, or the console
    // itself - in that order, most specific first.
    let snap_path = if args.no_patch || args.from_scene.is_some() {
        None
    } else {
        args.from_snap.clone().or_else(|| cfg.patch.snap_file.clone())
    };
    let mut names: std::collections::BTreeMap<u16, String> = Default::default();
    let mut extras: Vec<String> = Vec::new();
    let via_patch = snap_path.is_some();

    if let Some(path) = &args.from_scene {
        let scene = quscene::read(path)?;
        for w in &scene.warnings {
            println!("warning: {w}");
        }
        for channel in &scene.channels {
            if let quscene::Slot::Input(n) = channel.slot {
                if !channel.name.is_empty() {
                    names.insert(n, channel.name.clone());
                }
            }
        }
        if args.include_extras {
            // An unnamed FX return is only worth a track if the empties were
            // asked for, the same rule the inputs follow.
            extras = scene
                .extras()
                .into_iter()
                .filter(|(_, name)| args.include_unnamed || !name.is_empty())
                .map(|(label, name)| if name.is_empty() { label } else { name })
                .collect();
        }
        println!("{}: {}", path.display(), scene.summary());
    }

    match &snap_path {
        Some(path) => {
            let snap = snapfile::SnapFile::load(path)?;
            let group = args
                .output
                .clone()
                .unwrap_or_else(|| cfg.patch.output_group.clone());
            let slots = snap.outputs(&group);
            anyhow::ensure!(
                !slots.is_empty(),
                "no output group {group:?} in {} - run snap-info on it",
                path.display()
            );
            for slot in &slots {
                if !slot.name.is_empty() {
                    names.insert(slot.output, slot.name.clone());
                }
            }
            println!(
                "read {} names from {} ({}, {})",
                names.len(),
                path.display(),
                group,
                snapfile::group_label(&group)
            );
        }
        None if args.from_scene.is_some() => {}
        None => {
            let (wing, mut rx) = open_wing(&cfg).await?;
            wing.subscribe().await.ok();
            wing.query_all_names().await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(args.wait_secs.max(1));
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, rx.recv()).await {
                    Ok(Some(inc)) => {
                        if let Some(ch) = wing.channel_of_name_address(&inc.msg.addr) {
                            if let Some(name) = inc.msg.args.first().and_then(osc::as_str) {
                                names.insert(ch, name.trim().to_string());
                            }
                        }
                    }
                    _ => break,
                }
            }
            println!("read {} channel names from the console", names.len());
        }
    }

    let mut raw = Vec::new();
    for slot in first..=last {
        let name = names.get(&slot).cloned().unwrap_or_default();
        if name.is_empty() {
            if !args.include_unnamed {
                continue;
            }
            raw.push(if via_patch { format!("Out {slot}") } else { format!("Ch {slot}") });
        } else {
            raw.push(name);
        }
    }
    raw.extend(extras);
    let tracks = session::normalise_track_names(raw);
    anyhow::ensure!(
        !tracks.is_empty(),
        "nothing named in range {first}-{last}; pass --include-unnamed to create them anyway"
    );

    let report = session::create(&SessionRequest {
        parent_dir: args.dest,
        name: args.name,
        sample_rate: args.rate,
        tracks,
        template: args.template,
        connect_inputs: args.connect_inputs,
        styles: Vec::new(),
        allow_minimal: args.allow_minimal,
    })?;
    println!("created {} with {} tracks", report.session_file.display(), report.tracks);
    if let Some(t) = &report.template {
        println!("cloned from {}", t.display());
    }
    for w in &report.warnings {
        println!("warning: {w}");
    }
    Ok(())
}

struct WingSnapshotArgs {
    session: Option<PathBuf>,
    out: Option<PathBuf>,
    template: Option<PathBuf>,
    output: Option<String>,
    first_channel: Option<u16>,
    max_len: Option<usize>,
    include_busses: bool,
    apply: bool,
}

async fn cmd_wing_snapshot(path: &std::path::Path, args: WingSnapshotArgs) -> Result<()> {
    let cfg = Config::load(path)?;
    let session = args
        .session
        .or_else(|| cfg.livetrax.session_file.clone())
        .context("pass --session, or set livetrax.session_file in the config")?;

    let template = args.template.or_else(|| cfg.patch.snap_file.clone());
    let req = SnapshotRequest {
        session,
        first_channel: args.first_channel.unwrap_or(cfg.snapshot.first_channel),
        max_len: args.max_len.unwrap_or(cfg.names.max_len_wing),
        include_busses: args.include_busses || cfg.snapshot.include_busses,
        output_group: template
            .as_ref()
            .map(|_| args.output.clone().unwrap_or_else(|| cfg.patch.output_group.clone())),
        template,
    };
    let plan = snapshot::plan(&req)?;
    anyhow::ensure!(!plan.entries.is_empty(), "no tracks found in the session");

    println!("{} tracks from \"{}\"", plan.entries.len(), plan.session_name);
    for entry in &plan.entries {
        let note = if entry.name != entry.track { "  (truncated)" } else { "" };
        match entry.output {
            Some(out) => println!("  out {out:>3} -> ch {:>3}  {}{}", entry.channel, entry.name, note),
            None => println!("  ch {:>3}  {}{}", entry.channel, entry.name, note),
        }
    }

    if !plan.skipped.is_empty() {
        println!(
            "{} tracks skipped (their output carries no channel): {}",
            plan.skipped.len(),
            plan.skipped
                .iter()
                .map(|(out, track)| format!("out {out} \"{track}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if let Some(out) = &args.out {
        let report = snapshot::write(&plan, &req, &cfg.snapshot, &cfg.wing, out)?;
        match &report.template {
            Some(t) => println!(
                "rewrote {} name lines from {} into {}",
                report.written,
                t.display(),
                report.path.display()
            ),
            None => println!("wrote {} channels to {}", report.written, report.path.display()),
        }
        if !report.unmatched.is_empty() {
            println!(
                "warning: the template had no name line for channels {:?}",
                report.unmatched
            );
        }
    }

    if args.apply {
        let (wing, _rx) = open_wing(&cfg).await?;
        for entry in &plan.entries {
            wing.set_name(entry.channel, &entry.name).await?;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        println!("applied {} names to the console", plan.entries.len());
    }

    if args.out.is_none() && !args.apply {
        println!("(preview only - pass --out to write a file and/or --apply to push to the console)");
    }
    Ok(())
}

fn parse_range(raw: &str) -> Result<(u16, u16)> {
    let (a, b) = raw.split_once('-').unwrap_or((raw, raw));
    let first: u16 = a.trim().parse().with_context(|| format!("bad channel range {raw:?}"))?;
    let last: u16 = b.trim().parse().with_context(|| format!("bad channel range {raw:?}"))?;
    anyhow::ensure!(first >= 1 && last >= first, "bad channel range {raw:?}");
    Ok((first, last))
}

async fn cmd_strips(path: &std::path::Path, seconds: u64) -> Result<()> {
    let cfg = Config::load(path)?;
    let (daw, mut daw_rx) = open_daw(&cfg).await?;
    bridge::dump_strips(&daw, &mut daw_rx, seconds).await
}

fn cmd_markers(path: &std::path::Path) -> Result<()> {
    let cfg = Config::load(path)?;
    let Some(raw) = cfg.livetrax.session_file.as_ref() else {
        anyhow::bail!("set livetrax.session_file in {} first", path.display());
    };
    let file = markers::resolve_session_path(raw)?;
    let info = markers::parse_session(&file)?;
    let rate = info.sample_rate.unwrap_or(cfg.livetrax.sample_rate).max(1.0);
    let fps = cfg.timecode.fps.or(info.fps).unwrap_or_default();
    let offset_frames =
        (info.offset_samples as f64 / rate * fps.rate()).round() as i64;
    println!(
        "{} ({} markers, {} Hz, timecode {})",
        file.display(),
        info.markers.len(),
        rate,
        fps.label()
    );
    println!("{:>14}  {:>12}  {:>12}  name", "samples", "time", "timecode");
    for m in &info.markers {
        println!(
            "{:>14}  {:>12}  {:>12}  {}",
            m.start,
            hms(m.start as f64 / rate),
            timecode::from_samples(m.start, rate, fps, offset_frames),
            m.name
        );
    }
    Ok(())
}

/// Gather patch replies for a while, returning how many were ours.
async fn collect_patch(
    wing: &Wing,
    cfg: &Config,
    rx: &mut tokio::sync::mpsc::Receiver<Incoming>,
    model: &mut patch::PatchModel,
    names: &mut std::collections::BTreeMap<u16, String>,
    seconds: u64,
) -> usize {
    let mut replies = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds.max(1));
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Some(inc)) = tokio::time::timeout(remaining, rx.recv()).await else { break };
        if let Some(ch) = wing.channel_of_name_address(&inc.msg.addr) {
            if let Some(name) = inc.msg.args.first().and_then(osc::as_str) {
                names.insert(ch, name.trim().to_string());
                continue;
            }
        }
        if patch::absorb(model, &cfg.patch.live, &inc.msg.addr, &inc.msg.args) {
            replies += 1;
        }
    }
    replies
}

/// Query a running console for its patch - the live twin of `snap-info`.
async fn cmd_patch(
    path: &std::path::Path,
    output: Option<String>,
    seconds: u64,
    list_groups: bool,
) -> Result<()> {
    let cfg = Config::load(path)?;
    let group = output.unwrap_or_else(|| cfg.patch.output_group.clone());
    let (wing, mut rx) = open_wing(&cfg).await?;
    wing.subscribe().await.ok();
    wing.query_all_names().await.ok();

    let asked = wing
        .query_patch(&cfg.patch.live, &group, cfg.wing.channels)
        .await?;
    println!("asked the console {asked} questions about {group}; listening for {seconds}s\n");

    let mut model = patch::PatchModel::default();
    let mut names: std::collections::BTreeMap<u16, String> = Default::default();
    let mut replies =
        collect_patch(&wing, &cfg, &mut rx, &mut model, &mut names, seconds).await;

    // Sockets no channel owns may still carry a label on the console.
    let sockets = model.unclaimed_sockets(&group);
    if !sockets.is_empty() {
        wing.query_input_names(&cfg.patch.live, &sockets).await?;
        replies += collect_patch(&wing, &cfg, &mut rx, &mut model, &mut names, 2).await;
    }
    model.channel_names = names.into_iter().filter(|(_, n)| !n.is_empty()).collect();

    println!("{replies} patch replies, {} channel names", model.channel_names.len());
    if replies == 0 {
        anyhow::bail!(
            "the console answered nothing. Check that wing.host is right and that the \
             [patch.live] addresses match your firmware - `probe --target wing` shows what \
             it actually sends. A .snap file works offline in the meantime."
        );
    }
    if list_groups || !model.outputs.contains_key(&group) {
        println!("\ngroups the console answered for:");
        for g in model.groups() {
            println!("  {:<4} {}", g.id, g.summary());
        }
        if !list_groups {
            anyhow::bail!("no answers for output group {group:?}");
        }
        return Ok(());
    }

    println!("\n{} ({}):", patch::group_label(&group), group);
    println!("{:>4}  {:<10} {:<6} name", "out", "patched", "ch");
    for slot in model.slots(&group) {
        let ch = slot.channel.map(|c| c.to_string()).unwrap_or_default();
        println!("{:>4}  {:<10} {:<6} {}", slot.output, slot.source, ch, slot.name);
    }
    Ok(())
}

fn cmd_qu_scene(file: &std::path::Path, all: bool) -> Result<()> {
    let scene = quscene::read(file)?;
    println!("{}", file.display());
    println!("  {}", scene.summary());
    for w in &scene.warnings {
        println!("  warning: {w}");
    }
    if all {
        println!("\n{:<6} {:<12} {:<10} name", "slot#", "slot", "stored as");
    } else {
        println!("\n{:<12} {:<10} name", "slot", "stored as");
    }
    for channel in &scene.channels {
        let hidden = matches!(channel.slot, quscene::Slot::Other) || channel.name.is_empty();
        if hidden && !all {
            continue;
        }
        if all {
            println!(
                "{:<6} {:<12} {:<10} {}",
                channel.index,
                channel.slot.label(),
                format!("#{}", channel.id),
                channel.name
            );
        } else {
            println!(
                "{:<12} {:<10} {}",
                channel.slot.label(),
                format!("#{}", channel.id),
                channel.name
            );
        }
    }
    println!(
        "\nBuild a session from it with:\n  \
         wing-livetrax-bridge new-session --from-scene {} --dest ~/Music/Livetrax --name \"My Show\"",
        file.display()
    );
    Ok(())
}

fn cmd_snap_info(file: &std::path::Path, output: Option<&str>) -> Result<()> {
    let snap = snapfile::SnapFile::load(file)?;
    println!("{}", file.display());
    println!("  {}", snap.creator());
    let names = snap.channel_names();
    println!("  {} channels, {} named", snap.channel_count(), names.len());

    match output {
        None => {
            println!("\noutput groups (pass --output <id> to resolve one):");
            for group in snap.output_groups() {
                println!("  {:<4} {}", group.id, group.summary());
            }
            println!("\nnamed channels:");
            for (ch, name) in &names {
                println!("  ch {ch:>3}  {name}");
            }
        }
        Some(group) => {
            let slots = snap.outputs(group);
            anyhow::ensure!(
                !slots.is_empty(),
                "no output group {group:?} in this snapshot - try snap-info without --output"
            );
            println!("\n{} ({}):", snapfile::group_label(group), group);
            println!("{:>4}  {:<10} {:<6} name", "out", "patched", "ch");
            for slot in slots {
                let ch = slot.channel.map(|c| c.to_string()).unwrap_or_default();
                println!(
                    "{:>4}  {:<10} {:<6} {}",
                    slot.output, slot.source, ch, slot.name
                );
            }
        }
    }
    Ok(())
}

fn hms(seconds: f64) -> String {
    let total = seconds.max(0.0);
    let h = (total / 3600.0).floor() as u64;
    let m = ((total % 3600.0) / 60.0).floor() as u64;
    let s = total % 60.0;
    format!("{h:02}:{m:02}:{s:06.3}")
}

async fn cmd_probe(
    path: &std::path::Path,
    target: Target,
    seconds: u64,
    filter: Option<String>,
) -> Result<()> {
    let cfg = Config::load(path)?;
    let mut wing_rx = None;
    let mut daw_rx = None;

    if target != Target::Daw {
        let (wing, rx) = open_wing(&cfg).await?;
        wing.subscribe().await.ok();
        if cfg.names.enabled {
            wing.query_all_names().await.ok();
        }
        // Keep the subscription alive for the duration of the probe.
        tokio::spawn({
            let wing = wing.clone();
            let every = Duration::from_millis(cfg.wing.subscribe_interval_ms.max(500));
            async move {
                let mut tick = tokio::time::interval(every);
                loop {
                    tick.tick().await;
                    wing.subscribe().await.ok();
                }
            }
        });
        wing_rx = Some(rx);
    }
    if target != Target::Wing {
        let (daw, rx) = open_daw(&cfg).await?;
        daw.set_surface().await.ok();
        daw.request_strip_list().await.ok();
        tokio::spawn({
            let daw = daw.clone();
            let every = Duration::from_millis(cfg.livetrax.refresh_interval_ms.max(1_000));
            async move {
                let mut tick = tokio::time::interval(every);
                loop {
                    tick.tick().await;
                    daw.set_surface().await.ok();
                }
            }
        });
        daw_rx = Some(rx);
    }

    println!("Probing. Touch the control you care about on the console, or move the DAW transport.");
    let deadline = if seconds == 0 {
        None
    } else {
        Some(tokio::time::Instant::now() + Duration::from_secs(seconds))
    };

    loop {
        let inc = tokio::select! {
            Some(m) = async { match wing_rx.as_mut() { Some(rx) => rx.recv().await, None => None } } => ("WING", m),
            Some(m) = async { match daw_rx.as_mut() { Some(rx) => rx.recv().await, None => None } } => ("DAW ", m),
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = sleep_until(deadline) => return Ok(()),
        };
        if let Some(f) = &filter {
            if !inc.1.msg.addr.contains(f.as_str()) {
                continue;
            }
        }
        println!("{} {:<21} {}", inc.0, inc.1.from.to_string(), osc::render(&inc.1.msg));
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

async fn cmd_learn(path: &std::path::Path, seconds: u64) -> Result<()> {
    let cfg = Config::load(path)?;
    let (wing, mut rx) = open_wing(&cfg).await?;
    wing.subscribe().await.ok();
    tokio::spawn({
        let wing = wing.clone();
        let every = Duration::from_millis(cfg.wing.subscribe_interval_ms.max(500));
        async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                wing.subscribe().await.ok();
            }
        }
    });

    println!("Learn mode. Press a user button, then recall a scene. Ctrl-C when done.\n");
    let deadline = if seconds == 0 {
        None
    } else {
        Some(tokio::time::Instant::now() + Duration::from_secs(seconds))
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();

    loop {
        let inc = tokio::select! {
            Some(m) = rx.recv() => m,
            _ = tokio::signal::ctrl_c() => break,
            _ = sleep_until(deadline) => break,
        };
        let addr = inc.msg.addr.clone();
        if wing.channel_of_name_address(&addr).is_some() {
            continue; // name traffic is already understood
        }
        if !seen.insert(addr.clone()) {
            continue;
        }
        println!("# saw {}", osc::render(&inc.msg));
        println!("[[transport.buttons]]");
        println!("address = \"{addr}\"");
        println!("action = \"toggle_play\"   # play | stop | toggle_play | record_arm_toggle | add_marker | ...");
        println!();
        println!("# ...or, if this is your scene/snapshot indicator:");
        println!("# [scenes]");
        println!("# scene_address = \"{addr}\"\n");
    }
    Ok(())
}

async fn cmd_send(
    path: &std::path::Path,
    target: Target,
    address: &str,
    args: &[String],
) -> Result<()> {
    let cfg = Config::load(path)?;
    let parsed: Vec<OscType> = args.iter().map(|a| parse_arg(a)).collect();
    match target {
        Target::Wing => {
            let (wing, _rx) = open_wing(&cfg).await?;
            wing.send_raw(address, parsed).await?;
        }
        Target::Daw => {
            let (daw, _rx) = open_daw(&cfg).await?;
            daw.link.send(address.to_string(), parsed).await?;
        }
        Target::Both => anyhow::bail!("--target must be wing or daw for send"),
    }
    // Give the socket a moment to flush before the process exits.
    tokio::time::sleep(Duration::from_millis(150)).await;
    Ok(())
}

fn parse_arg(raw: &str) -> OscType {
    if let Some(v) = raw.strip_prefix("i:") {
        return OscType::Int(v.parse().unwrap_or(0));
    }
    if let Some(v) = raw.strip_prefix("f:") {
        return OscType::Float(v.parse().unwrap_or(0.0));
    }
    if let Some(v) = raw.strip_prefix("s:") {
        return OscType::String(v.to_string());
    }
    if let Ok(i) = raw.parse::<i32>() {
        return OscType::Int(i);
    }
    if let Ok(f) = raw.parse::<f32>() {
        return OscType::Float(f);
    }
    OscType::String(raw.to_string())
}

fn cmd_init(output: &std::path::Path) -> Result<()> {
    if output.exists() {
        anyhow::bail!("{} already exists", output.display());
    }
    std::fs::write(output, include_str!("../config.example.toml"))
        .with_context(|| format!("writing {}", output.display()))?;
    println!("wrote {}", output.display());
    println!("Edit wing.host and livetrax.host, then run: wing-livetrax-bridge probe");
    Ok(())
}

// ------------------------------------------------------------ patch sheet ---

fn cmd_patch_template(output: &std::path::Path) -> Result<()> {
    if output.exists() {
        anyhow::bail!("{} already exists - pick another name", output.display());
    }
    std::fs::write(output, sheet::template())
        .with_context(|| format!("writing {}", output.display()))?;
    println!("wrote {}", output.display());
    println!(
        "Open it in Excel, Numbers or Sheets, replace the example rows with your patch,\n\
         and save it as CSV. Then:\n\n  \
         wing-livetrax-bridge build --sheet {} --dest ~/Music/Livetrax --name \"My Show\"",
        output.display()
    );
    Ok(())
}

struct BuildArgs {
    sheet: PathBuf,
    desk: String,
    dest: Option<PathBuf>,
    name: String,
    snap: Option<PathBuf>,
    base: Option<PathBuf>,
    output: Option<String>,
    template: Option<PathBuf>,
    rate: u32,
    connect_inputs: bool,
    arm: bool,
    allow_minimal: bool,
    keep_unlisted_outputs: bool,
    label_sources: bool,
    dry_run: bool,
}

fn cmd_build(path: &std::path::Path, args: BuildArgs) -> Result<()> {
    let cfg = Config::load(path)?;
    let desk = patchbuild::Desk::from_name(&args.desk).with_context(|| {
        format!(
            "{:?} is not a desk I know - try {}",
            args.desk,
            patchbuild::Desk::ALL.iter().map(|d| d.slug()).collect::<Vec<_>>().join(", ")
        )
    })?;
    if !desk.writes_snapshot() {
        anyhow::ensure!(
            args.base.is_none(),
            "{} {} has no snapshot to overlay, so --base has nothing to do",
            desk.article(),
            desk.label()
        );
        anyhow::ensure!(
            args.snap.is_none(),
            "{} {} has no snapshot format, so there is nothing to write to --snap",
            desk.article(),
            desk.label()
        );
        anyhow::ensure!(
            args.dest.is_some(),
            "{} {} builds the session only, so --dest is needed to say where it goes",
            desk.article(),
            desk.label()
        );
    }
    let sheet = sheet::read(&args.sheet)?;
    let group = args
        .output
        .clone()
        .unwrap_or_else(|| cfg.patch.output_group.clone())
        .to_uppercase();

    let built = patchbuild::build(
        &sheet,
        &patchbuild::BuildRequest {
            desk,
            base: args.base.clone(),
            record_group: group.clone(),
            label_sources: args.label_sources,
            keep_unlisted_outputs: args.keep_unlisted_outputs,
        },
    )?;
    let report = &built.report;

    if desk.writes_snapshot() {
        println!(
            "{}: {} channels, {} tracks on {}",
            args.sheet.display(),
            report.channels.len(),
            report.tracks.len(),
            group
        );
    } else {
        println!(
            "{}: {} channels, {} tracks for {} {}",
            args.sheet.display(),
            report.channels.len(),
            report.tracks.len(),
            desk.article(),
            desk.label()
        );
    }
    if !sheet.unknown_columns.is_empty() {
        println!("carried through untouched: {}", sheet.unknown_columns.join(", "));
    }
    if desk.writes_snapshot() {
        match &report.base {
            Some(p) => println!("overlaid on {}", p.display()),
            None => println!("overlaid on a factory console"),
        }
        println!("{} nodes moved", report.changes.len());
        if report.cleared > 0 {
            println!("{} {group} outputs the sheet does not use were switched off", report.cleared);
        }
    }

    if args.dry_run {
        for change in &report.changes {
            println!("  {change}");
        }
        for w in &report.warnings {
            println!("warning: {w}");
        }
        println!("\n(dry run - nothing was written)");
        return Ok(());
    }

    // The session folder is made first: it refuses to overwrite, and a snapshot
    // written beside a folder that then fails to appear is just litter.
    let mut session_folder: Option<PathBuf> = None;
    if let Some(dest) = &args.dest {
        let names = patchbuild::daw_names(&report.tracks);
        let styles = report
            .tracks
            .iter()
            .map(|t| session::TrackStyle {
                colour: t.colour.and_then(patchbuild::track_colour),
                rec_arm: args.arm && t.channel.is_some(),
            })
            .collect();
        let created = session::create(&SessionRequest {
            parent_dir: dest.clone(),
            name: args.name.clone(),
            sample_rate: args.rate,
            tracks: names,
            template: args.template.clone(),
            connect_inputs: args.connect_inputs,
            allow_minimal: args.allow_minimal,
            styles,
        })?;
        println!(
            "created {} with {} tracks",
            created.session_file.display(),
            created.tracks
        );
        if let Some(t) = &created.template {
            println!("cloned from {}", t.display());
        }
        for w in &created.warnings {
            println!("warning: {w}");
        }
        session_folder = Some(created.folder);
    }

    if built.snapshot.is_none() {
        for w in &report.warnings {
            println!("warning: {w}");
        }
        println!(
            "\nOpen the session with Session > Open in LiveTrax. Set the {} up from the sheet \n\
             by hand, or from a scene you already have - it has no file format this can write.",
            desk.label()
        );
        return Ok(());
    }

    // The snapshot belongs with the show, so it goes inside the session folder
    // when there is one and beside the sheet when there is not.
    let snap_path = args.snap.clone().unwrap_or_else(|| {
        let file = format!("{}.snap", args.name);
        match &session_folder {
            Some(folder) => folder.join(file),
            None => args
                .sheet
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.join(&file))
                .unwrap_or_else(|| PathBuf::from(file)),
        }
    });
    built.write_snap(&snap_path)?;
    println!("wrote {}", snap_path.display());

    for w in &report.warnings {
        println!("warning: {w}");
    }
    println!(
        "\nLoad the snapshot from the console's library (or open it in WING-Edit), and open\n\
         the session with Session > Open in LiveTrax."
    );
    Ok(())
}
