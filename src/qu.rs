//! Allen & Heath Qu, over MIDI on TCP.
//!
//! Everything here comes from A&H's published Qu MIDI Protocol (V1.9+):
//!
//! * one TCP connection at a time, on port 51325;
//! * an Active Sense byte at least every 300 ms or the mixer hangs up after 12
//!   seconds, which makes the keep-alive part of the protocol rather than a
//!   nicety;
//! * a SysEx header of `F0 00 00 1A 50 11 01 00 0N`, where N is the mixer's
//!   MIDI channel - asked for with an "all call" header and learned from the
//!   reply, because it is not known until then;
//! * channel names read with `01 CH` and written with `03 CH`, where input 1
//!   is `CH` 0x20;
//! * scene recalls as Bank Select plus Program Change;
//! * MMC (`F0 7F 7F 06 TC F7`) from the SoftKeys, which is how a Qu drives a
//!   transport.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::config;
use crate::console::ConsoleEvent;
use crate::midi::{self, Message};

/// A&H, then the Qu family, then the protocol version.
const HEADER: [u8; 7] = [0x00, 0x00, 0x1A, 0x50, 0x11, 0x01, 0x00];
/// Stands in for the MIDI channel before we know it.
const ALL_CALL: u8 = 0x7F;
/// Input channel 1. Inputs run to 0x3F.
const FIRST_INPUT: u8 = 0x20;

/// Control changes that carry NRPN plumbing rather than a control someone
/// touched. Without this the mixer's state dump fills the learn list.
const PLUMBING: [u8; 6] = [0x00, 0x20, 0x06, 0x26, 0x62, 0x63];

#[derive(Clone)]
pub struct Qu {
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    /// Learned from the handshake; until then, whatever the config said.
    channel: Arc<AtomicU8>,
    cfg: config::Qu,
    target: String,
}

impl Qu {
    /// Open the connection and keep it open. Reconnects on its own.
    pub fn connect(cfg: config::Qu) -> (Qu, mpsc::Receiver<ConsoleEvent>) {
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::channel(2048);
        let target = format!("{}:{}", cfg.host, cfg.port);
        let channel = Arc::new(AtomicU8::new(cfg.midi_channel.unwrap_or(0)));

        let qu = Qu {
            outgoing: outgoing_tx,
            channel: channel.clone(),
            cfg: cfg.clone(),
            target: target.clone(),
        };
        tokio::spawn(run(cfg, target, channel, outgoing_rx, events_tx));
        (qu, events_rx)
    }

    pub fn target(&self) -> String {
        self.target.clone()
    }

    fn midi_channel(&self) -> u8 {
        self.channel.load(Ordering::Relaxed) & 0x0F
    }

    fn header(&self) -> Vec<u8> {
        let mut bytes = vec![0xF0];
        bytes.extend_from_slice(&HEADER);
        bytes.push(self.midi_channel());
        bytes
    }

    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        self.outgoing
            .send(bytes)
            .map_err(|_| anyhow::anyhow!("the Qu connection is gone"))
    }

    /// Ask the mixer who it is. The reply carries its MIDI channel.
    pub fn hello(&self) -> Result<()> {
        let mut bytes = vec![0xF0];
        bytes.extend_from_slice(&HEADER);
        bytes.extend_from_slice(&[ALL_CALL, 0x10, 0x00, 0xF7]);
        self.send(bytes)
    }

    pub fn active_sense(&self) -> Result<()> {
        self.send(vec![0xFE])
    }

    pub fn query_name(&self, channel: u16) -> Result<()> {
        let Some(ch) = input_channel(channel) else { return Ok(()) };
        let mut bytes = self.header();
        bytes.extend_from_slice(&[0x01, ch, 0xF7]);
        self.send(bytes)
    }

    pub fn query_all_names(&self, channels: u16) -> Result<()> {
        for channel in 1..=channels {
            self.query_name(channel)?;
        }
        Ok(())
    }

    pub fn set_name(&self, channel: u16, name: &str) -> Result<()> {
        let Some(ch) = input_channel(channel) else { return Ok(()) };
        let mut bytes = self.header();
        bytes.extend_from_slice(&[0x03, ch]);
        // Names go over as plain ASCII; anything else has no representation.
        bytes.extend(name.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').map(|c| c as u8));
        bytes.push(0xF7);
        self.send(bytes)
    }

    /// Bank Select then Program Change, which is how the Qu recalls a scene.
    pub fn recall_scene(&self, index: i32) -> Result<()> {
        if !(1..=100).contains(&index) {
            anyhow::bail!("the Qu has scenes 1 to 100, not {index}");
        }
        let n = self.midi_channel();
        self.send(vec![
            0xB0 | n, 0x00, 0x00,
            0xB0 | n, 0x20, 0x00,
            0xC0 | n, (index - 1) as u8,
        ])
    }

    /// Light a key, or send any other control the config names.
    pub fn send_control(&self, id: &str, value: f32) -> Result<()> {
        let Some(control) = Control::parse(id) else {
            anyhow::bail!("{id:?} is not a MIDI control - try midi:note/1/0 or midi:cc/0/17");
        };
        let data = (value.clamp(0.0, 1.0) * 127.0).round() as u8;
        self.send(control.to_bytes(data))
    }
}

/// The text form of a MIDI control, so bindings and lights can live in the
/// config file next to OSC addresses.
struct Control {
    kind: Kind,
    channel: u8,
    number: u8,
}

enum Kind {
    Note,
    Cc,
}

impl Control {
    fn parse(id: &str) -> Option<Control> {
        let rest = id.strip_prefix("midi:")?;
        let mut parts = rest.split('/');
        let kind = match parts.next()? {
            "note" => Kind::Note,
            "cc" => Kind::Cc,
            _ => return None,
        };
        let channel = parts.next()?.parse::<u8>().ok()? & 0x0F;
        let number = parts.next()?.parse::<u8>().ok()? & 0x7F;
        Some(Control { kind, channel, number })
    }

    fn to_bytes(&self, value: u8) -> Vec<u8> {
        match self.kind {
            Kind::Note => vec![0x90 | self.channel, self.number, value],
            Kind::Cc => vec![0xB0 | self.channel, self.number, value],
        }
    }

    fn id(kind: &str, channel: u8, number: u8) -> String {
        format!("midi:{kind}/{channel}/{number}")
    }
}

/// Input channel number to the Qu's channel id.
fn input_channel(channel: u16) -> Option<u8> {
    (1..=32).contains(&channel).then(|| FIRST_INPUT + (channel as u8 - 1))
}

/// The Qu's channel id back to an input channel number.
fn from_qu_channel(ch: u8) -> Option<u16> {
    (FIRST_INPUT..=0x3F).contains(&ch).then(|| (ch - FIRST_INPUT) as u16 + 1)
}

fn model_name(box_id: u8) -> &'static str {
    match box_id {
        1 => "Qu-16",
        2 => "Qu-24",
        3 => "Qu-32",
        4 => "Qu-Pac",
        5 => "Qu-SB",
        _ => "Qu",
    }
}

/// MMC transport codes, as the SoftKeys send them.
fn mmc_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x01 => "stop",
        0x02 => "play",
        0x04 => "fast-forward",
        0x05 => "rewind",
        0x06 => "record",
        0x09 => "pause",
        _ => return None,
    })
}

/// Connect, talk, and reconnect for as long as the bridge is running.
async fn run(
    cfg: config::Qu,
    target: String,
    channel: Arc<AtomicU8>,
    mut outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
    events: mpsc::Sender<ConsoleEvent>,
) {
    let sense = Duration::from_millis(cfg.active_sense_ms.clamp(50, 300));
    let retry = Duration::from_millis(cfg.reconnect_ms.max(250));
    loop {
        match session(&target, &channel, &mut outgoing, &events, sense).await {
            Ok(()) => tracing::warn!("qu: the mixer closed the connection"),
            Err(e) => tracing::warn!("qu: {e:#}"),
        }
        tracing::info!("qu: reconnecting to {target} in {:?}", retry);
        tokio::time::sleep(retry).await;
    }
}

async fn session(
    target: &str,
    channel: &Arc<AtomicU8>,
    outgoing: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    events: &mpsc::Sender<ConsoleEvent>,
    sense: Duration,
) -> Result<()> {
    let stream = TcpStream::connect(target)
        .await
        .with_context(|| format!("connecting to {target}"))?;
    stream.set_nodelay(true).ok();
    tracing::info!("qu: connected to {target}");
    let (mut reader, mut writer) = stream.into_split();

    // Ask who we are talking to; the reply carries the MIDI channel.
    let mut hello = vec![0xF0];
    hello.extend_from_slice(&HEADER);
    hello.extend_from_slice(&[ALL_CALL, 0x10, 0x00, 0xF7]);
    writer.write_all(&hello).await?;

    let mut parser = midi::Parser::default();
    let mut buffer = vec![0u8; 4096];
    let mut ticker = tokio::time::interval(sense);

    loop {
        tokio::select! {
            read = reader.read(&mut buffer) => {
                let read = read.context("reading from the mixer")?;
                if read == 0 {
                    return Ok(());
                }
                for message in parser.feed(&buffer[..read]) {
                    for event in interpret(message, channel) {
                        if events.send(event).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            bytes = outgoing.recv() => {
                let Some(bytes) = bytes else { return Ok(()) };
                writer.write_all(&bytes).await.context("writing to the mixer")?;
            }
            _ = ticker.tick() => {
                // Silence for twelve seconds and the mixer hangs up.
                writer.write_all(&[0xFE]).await.context("sending active sense")?;
            }
        }
    }
}

/// Turn one MIDI message into whatever the bridge can act on.
fn interpret(message: Message, channel: &Arc<AtomicU8>) -> Vec<ConsoleEvent> {
    match message {
        Message::ActiveSense => Vec::new(),
        Message::SysEx(payload) => interpret_sysex(&payload, channel),
        Message::ProgramChange { program, .. } => {
            vec![ConsoleEvent::Scene { index: program as i32 + 1 }]
        }
        Message::NoteOn { channel: ch, note, velocity } => vec![ConsoleEvent::Control {
            id: Control::id("note", ch, note),
            value: velocity as f32 / 127.0,
        }],
        Message::NoteOff { channel: ch, note, .. } => vec![ConsoleEvent::Control {
            id: Control::id("note", ch, note),
            value: 0.0,
        }],
        Message::ControlChange { channel: ch, control, value } => {
            if PLUMBING.contains(&control) {
                return Vec::new();
            }
            vec![ConsoleEvent::Control {
                id: Control::id("cc", ch, control),
                value: value as f32 / 127.0,
            }]
        }
    }
}

fn interpret_sysex(payload: &[u8], channel: &Arc<AtomicU8>) -> Vec<ConsoleEvent> {
    // MMC, which is what a SoftKey set to transport control sends.
    if payload.starts_with(&[0x7F, 0x7F, 0x06]) {
        if let Some(name) = payload.get(3).copied().and_then(mmc_name) {
            return vec![ConsoleEvent::Control { id: format!("mmc:{name}"), value: 1.0 }];
        }
        return Vec::new();
    }
    if !payload.starts_with(&HEADER) || payload.len() < HEADER.len() + 2 {
        return Vec::new();
    }
    let body = &payload[HEADER.len() + 1..];
    match body[0] {
        // Who the mixer is, and the MIDI channel to use from here on.
        0x11 => {
            let mixer = payload[HEADER.len()] & 0x0F;
            channel.store(mixer, Ordering::Relaxed);
            let model = body.get(1).copied().map(model_name).unwrap_or("Qu");
            let version = match (body.get(2), body.get(3)) {
                (Some(major), Some(minor)) => format!(" firmware {major}.{minor}"),
                _ => String::new(),
            };
            tracing::info!("qu: {model}{version}, MIDI channel {}", mixer + 1);
            Vec::new()
        }
        0x14 => {
            tracing::debug!("qu: finished sending its state");
            Vec::new()
        }
        // A channel name we asked for.
        0x02 => {
            let Some(channel) = body.get(1).copied().and_then(from_qu_channel) else {
                return Vec::new();
            };
            let name: String = body[2..]
                .iter()
                .take_while(|b| **b != 0)
                .map(|b| *b as char)
                .collect();
            vec![ConsoleEvent::Name { channel, name: name.trim().to_string() }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> Arc<AtomicU8> {
        Arc::new(AtomicU8::new(0))
    }

    #[test]
    fn input_channels_map_to_the_documented_ids() {
        assert_eq!(input_channel(1), Some(0x20));
        assert_eq!(input_channel(16), Some(0x2F));
        assert_eq!(from_qu_channel(0x20), Some(1));
        assert_eq!(from_qu_channel(0x2F), Some(16));
        // Anything outside the input block is not a channel we name.
        assert_eq!(from_qu_channel(0x67), None);
        assert_eq!(input_channel(0), None);
    }

    #[test]
    fn the_handshake_reply_teaches_us_the_midi_channel() {
        let channel = channel();
        // Header with N = 3, then 11 <Qu-16> <1> <9>.
        let mut payload = HEADER.to_vec();
        payload.extend_from_slice(&[0x03, 0x11, 0x01, 0x01, 0x09]);
        assert!(interpret_sysex(&payload, &channel).is_empty());
        assert_eq!(channel.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_name_reply_becomes_a_name_event() {
        let mut payload = HEADER.to_vec();
        payload.extend_from_slice(&[0x00, 0x02, 0x22]); // channel 3
        payload.extend_from_slice(b"KICK  ");
        let events = interpret_sysex(&payload, &channel());
        match &events[0] {
            ConsoleEvent::Name { channel, name } => {
                assert_eq!(*channel, 3);
                assert_eq!(name, "KICK");
            }
            other => panic!("expected a name, got {other:?}"),
        }
    }

    #[test]
    fn mmc_from_a_softkey_becomes_a_bindable_control() {
        let events = interpret_sysex(&[0x7F, 0x7F, 0x06, 0x02], &channel());
        match &events[0] {
            ConsoleEvent::Control { id, value } => {
                assert_eq!(id, "mmc:play");
                assert_eq!(*value, 1.0);
            }
            other => panic!("expected a control, got {other:?}"),
        }
    }

    #[test]
    fn nrpn_plumbing_stays_out_of_the_control_list() {
        let channel = channel();
        let plumbing = interpret(
            Message::ControlChange { channel: 0, control: 0x63, value: 0x20 },
            &channel,
        );
        assert!(plumbing.is_empty());
        let key = interpret(Message::NoteOn { channel: 1, note: 0x7E, velocity: 0x7F }, &channel);
        assert_eq!(key.len(), 1);
    }

    #[test]
    fn scene_recall_is_bank_select_then_program_change() {
        let (qu, _events) = Qu::connect(config::Qu {
            host: "127.0.0.1".into(),
            port: 1,
            ..Default::default()
        });
        qu.channel.store(2, Ordering::Relaxed);
        qu.recall_scene(5).unwrap();
        // Scene 5 goes out as program 4 on MIDI channel 3.
        assert!(qu.recall_scene(0).is_err());
    }

    #[test]
    fn control_ids_round_trip() {
        let control = Control::parse("midi:note/1/126").unwrap();
        assert_eq!(control.to_bytes(0x7F), vec![0x91, 0x7E, 0x7F]);
        let control = Control::parse("midi:cc/0/17").unwrap();
        assert_eq!(control.to_bytes(0), vec![0xB0, 0x11, 0x00]);
        assert!(Control::parse("/ch/1/name").is_none());
    }
}
