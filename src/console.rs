//! The console, whichever one it is.
//!
//! A WING speaks OSC over UDP; a Qu speaks MIDI over TCP. What the bridge
//! actually needs from either is small - names, scene recalls, controls being
//! pressed, and a way to write back - so both are wrapped up here and
//! everything downstream works in [`ConsoleEvent`]s.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::{Arg, Config, ConsoleKind};
use crate::osc::OscLink;
use crate::qu::Qu;
use crate::wing::Wing;

/// Something the console did.
#[derive(Debug, Clone)]
pub enum ConsoleEvent {
    Name { channel: u16, name: String },
    /// A control someone touched. The id is what a binding refers to: an OSC
    /// address on a WING, `midi:note/1/126` or `mmc:play` on a Qu.
    Control { id: String, value: f32 },
    Scene { index: i32 },
    /// A WING message that is not a name: the bridge still needs the raw form
    /// for patch replies and address matching.
    Osc(rosc::OscMessage),
}

#[derive(Clone)]
pub enum Console {
    Wing(Wing),
    Qu(Qu),
}

impl Console {
    /// Open whichever console the configuration names.
    pub async fn open(cfg: &Config) -> Result<(Console, mpsc::Receiver<ConsoleEvent>)> {
        match cfg.console.kind {
            ConsoleKind::Wing => {
                let remote = resolve(&cfg.wing.host, cfg.wing.port).await?;
                let local = SocketAddr::from(([0, 0, 0, 0], cfg.wing.local_port));
                let (link, mut incoming) = OscLink::bind("wing", local, remote).await?;
                let wing = Wing::new(link, cfg.wing.clone());
                let (tx, events) = mpsc::channel(2048);
                let names = wing.clone();
                // Names are recognised here; everything else goes through raw.
                tokio::spawn(async move {
                    while let Some(message) = incoming.recv().await {
                        let event = match names.channel_of_name_address(&message.msg.addr) {
                            Some(channel) => match message.msg.args.first().and_then(crate::osc::as_str) {
                                Some(name) => ConsoleEvent::Name {
                                    channel,
                                    name: name.trim().to_string(),
                                },
                                None => ConsoleEvent::Osc(message.msg),
                            },
                            None => ConsoleEvent::Osc(message.msg),
                        };
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                });
                Ok((Console::Wing(wing), events))
            }
            ConsoleKind::Qu => {
                let (qu, events) = Qu::connect(cfg.qu.clone());
                Ok((Console::Qu(qu), events))
            }
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Console::Wing(_) => "WING",
            Console::Qu(_) => "Qu",
        }
    }

    pub fn target(&self) -> String {
        match self {
            Console::Wing(wing) => wing.link.remote().to_string(),
            Console::Qu(qu) => qu.target(),
        }
    }

    /// The WING behind this console, for the things only it can do.
    pub fn wing(&self) -> Option<&Wing> {
        match self {
            Console::Wing(wing) => Some(wing),
            Console::Qu(_) => None,
        }
    }

    /// Say hello: subscribe on a WING, ask a Qu who it is.
    pub async fn start(&self) -> Result<()> {
        match self {
            Console::Wing(wing) => wing.subscribe().await,
            Console::Qu(qu) => qu.hello(),
        }
    }

    /// Renew the subscription, or keep the Qu's connection from timing out.
    pub async fn keepalive(&self) -> Result<()> {
        match self {
            Console::Wing(wing) => wing.subscribe().await,
            Console::Qu(qu) => qu.active_sense(),
        }
    }

    /// How often that has to happen. The Qu hangs up after twelve seconds of
    /// silence, so this is not a formality.
    pub fn keepalive_interval(&self, cfg: &Config) -> Duration {
        match self {
            Console::Wing(_) => {
                Duration::from_millis(cfg.wing.subscribe_interval_ms.max(500))
            }
            Console::Qu(_) => Duration::from_millis(cfg.qu.active_sense_ms.clamp(50, 300)),
        }
    }

    pub async fn query_all_names(&self, channels: u16) -> Result<()> {
        match self {
            Console::Wing(wing) => wing.query_all_names().await,
            Console::Qu(qu) => qu.query_all_names(channels),
        }
    }

    pub async fn set_name(&self, channel: u16, name: &str) -> Result<()> {
        match self {
            Console::Wing(wing) => wing.set_name(channel, name).await,
            Console::Qu(qu) => qu.set_name(channel, name),
        }
    }

    pub async fn recall_scene(&self, address: &str, index: i32) -> Result<()> {
        match self {
            Console::Wing(wing) => wing.recall_scene(address, index).await,
            Console::Qu(qu) => qu.recall_scene(index),
        }
    }

    /// Write to a control: a light, an indicator, anything the config names.
    pub async fn send_control(&self, id: &str, arg: &Arg) -> Result<()> {
        match self {
            Console::Wing(wing) => wing.send_raw(id, vec![arg.to_osc()]).await,
            Console::Qu(qu) => qu.send_control(id, level(arg)),
        }
    }
}

/// A configured value as a 0-1 level, so `on = 1` lights a MIDI key fully
/// rather than sending it a velocity of one.
fn level(arg: &Arg) -> f32 {
    match arg {
        Arg::Bool(on) => *on as u8 as f32,
        Arg::Int(value) => {
            if *value > 1 {
                *value as f32 / 127.0
            } else {
                *value as f32
            }
        }
        Arg::Float(value) => *value,
        Arg::Str(_) => 1.0,
    }
}

pub async fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    if let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() {
        return Ok(addr);
    }
    tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolving {host}:{port}"))?
        .next()
        .with_context(|| format!("no address for {host}:{port}"))
}
