//! Thin async OSC/UDP endpoint used for both the console and the DAW.

use anyhow::{Context, Result};
use rosc::{OscMessage, OscPacket, OscType};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct Incoming {
    pub from: SocketAddr,
    pub msg: OscMessage,
}

#[derive(Clone)]
pub struct OscLink {
    sock: Arc<UdpSocket>,
    remote: SocketAddr,
    pub label: &'static str,
}

impl OscLink {
    /// Bind a local socket and start the receive pump.
    pub async fn bind(
        label: &'static str,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let sock = UdpSocket::bind(local)
            .await
            .with_context(|| format!("binding {label} socket on {local}"))?;
        let sock = Arc::new(sock);
        let (tx, rx) = mpsc::channel(2048);
        let recv_sock = sock.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            loop {
                match recv_sock.recv_from(&mut buf).await {
                    Ok((n, from)) => match rosc::decoder::decode_udp(normalise(&mut buf[..n])) {
                        Ok((_rest, packet)) => {
                            let mut msgs = Vec::new();
                            flatten(packet, &mut msgs);
                            for msg in msgs {
                                if tx.send(Incoming { from, msg }).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => tracing::debug!(%from, "{label}: undecodable packet: {e:?}"),
                    },
                    Err(e) => {
                        tracing::warn!("{label}: socket error: {e}");
                        return;
                    }
                }
            }
        });
        tracing::info!("{label}: listening on {}, talking to {remote}", sock.local_addr()?);
        Ok((Self { sock, remote, label }, rx))
    }

    pub fn remote(&self) -> SocketAddr { self.remote }

    pub async fn send(&self, addr: impl Into<String>, args: Vec<OscType>) -> Result<()> {
        self.send_msg(OscMessage { addr: addr.into(), args }).await
    }

    pub async fn send_msg(&self, msg: OscMessage) -> Result<()> {
        tracing::trace!("{} <- {} {:?}", self.label, msg.addr, msg.args);
        let bytes = rosc::encoder::encode(&OscPacket::Message(msg))
            .context("encoding OSC message")?;
        self.sock
            .send_to(&bytes, self.remote)
            .await
            .with_context(|| format!("sending to {}", self.remote))?;
        Ok(())
    }
}

/// Ardour-family DAWs answer `/strip/list` with an OSC 1.0 `#reply` packet.
/// That address does not start with '/', so strict decoders (rosc included)
/// reject the whole datagram - rewrite it to `/reply` before decoding. Real
/// `#bundle` packets are left alone.
fn normalise(buf: &mut [u8]) -> &[u8] {
    if buf.starts_with(b"#reply") {
        buf[0] = b'/';
    }
    buf
}

fn flatten(packet: OscPacket, out: &mut Vec<OscMessage>) {
    match packet {
        OscPacket::Message(m) => out.push(m),
        OscPacket::Bundle(b) => {
            for p in b.content {
                flatten(p, out);
            }
        }
    }
}

// ------------------------------------------------------------- arg helpers --

pub fn as_f32(arg: &OscType) -> Option<f32> {
    match arg {
        OscType::Int(i) => Some(*i as f32),
        OscType::Long(i) => Some(*i as f32),
        OscType::Float(f) => Some(*f),
        OscType::Double(d) => Some(*d as f32),
        OscType::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

pub fn as_i64(arg: &OscType) -> Option<i64> {
    match arg {
        OscType::Int(i) => Some(*i as i64),
        OscType::Long(i) => Some(*i),
        OscType::Float(f) => Some(*f as i64),
        OscType::Double(d) => Some(*d as i64),
        OscType::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

pub fn as_str(arg: &OscType) -> Option<&str> {
    match arg {
        OscType::String(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Human-readable one-liner used by `probe`/`learn` and trace logs.
pub fn render(msg: &OscMessage) -> String {
    let args: Vec<String> = msg.args.iter().map(render_arg).collect();
    if args.is_empty() {
        msg.addr.clone()
    } else {
        format!("{} {}", msg.addr, args.join(" "))
    }
}

pub fn render_arg(arg: &OscType) -> String {
    match arg {
        OscType::Int(i) => format!("i:{i}"),
        OscType::Long(i) => format!("h:{i}"),
        OscType::Float(f) => format!("f:{f}"),
        OscType::Double(d) => format!("d:{d}"),
        OscType::String(s) => format!("s:{s:?}"),
        OscType::Bool(b) => format!("T/F:{b}"),
        OscType::Blob(b) => format!("b:[{} bytes]", b.len()),
        OscType::Char(c) => format!("c:{c}"),
        OscType::Nil => "nil".into(),
        OscType::Inf => "inf".into(),
        other => format!("{other:?}"),
    }
}

/// Substitute `{key}` placeholders in an address template.
pub fn template(tpl: &str, vars: &[(&str, String)]) -> String {
    let mut out = tpl.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Inverse of `template` for a name-then-number pair, e.g. the template
/// `/io/out/{grp}/{n}/grp` against `/io/out/USB/17/grp` gives ("USB", 17).
pub fn parse_pair(tpl: &str, first: &str, second: &str, addr: &str) -> Option<(String, u16)> {
    let (head, rest_tpl) = tpl.split_once(&format!("{{{first}}}"))?;
    let (middle, tail) = rest_tpl.split_once(&format!("{{{second}}}"))?;
    let rest = addr.strip_prefix(head)?;
    let (name, rest) = rest.split_once(middle)?;
    let number = if tail.is_empty() { rest } else { rest.strip_suffix(tail)? };
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some((name.to_string(), number.parse::<u16>().ok()?))
}

/// Inverse of `template` for a single numeric placeholder: given the template
/// `/ch/{ch}/name` and the address `/ch/7/name`, returns 7.
pub fn parse_template(tpl: &str, key: &str, addr: &str) -> Option<u32> {
    let marker = format!("{{{key}}}");
    let (head, tail) = tpl.split_once(&marker)?;
    let rest = addr.strip_prefix(head)?;
    let num = if tail.is_empty() { rest } else { rest.strip_suffix(tail)? };
    num.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape LiveTrax uses to answer /strip/list.
    fn reply_packet() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"#reply\0\0");
        buf.extend_from_slice(b",si\0");
        buf.extend_from_slice(b"AT\0\0");
        buf.extend_from_slice(&7i32.to_be_bytes());
        buf
    }

    #[test]
    fn strict_decoding_rejects_hash_reply() {
        // Guards the reason `normalise` exists.
        assert!(rosc::decoder::decode_udp(&reply_packet()).is_err());
    }

    #[tokio::test]
    async fn hash_reply_arrives_as_slash_reply() {
        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (link, mut rx) = OscLink::bind(
            "test",
            "127.0.0.1:0".parse().unwrap(),
            peer.local_addr().unwrap(),
        )
        .await
        .unwrap();
        // Make the peer learn our address, then answer like the DAW does.
        link.send("/strip/list", vec![]).await.unwrap();
        let mut buf = [0u8; 1024];
        let (_, from) = peer.recv_from(&mut buf).await.unwrap();
        peer.send_to(&reply_packet(), from).await.unwrap();

        let inc = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inc.msg.addr, "/reply");
        assert_eq!(inc.msg.args[0], OscType::String("AT".into()));
        assert_eq!(inc.msg.args[1], OscType::Int(7));
    }
}
