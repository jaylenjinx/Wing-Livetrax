//! Harrison LiveTrax 3 side of the bridge.
//!
//! LiveTrax inherits Ardour's OSC control surface (UDP 3819 by default): a
//! surface announces itself with `/set_surface`, then receives strip and
//! transport feedback for as long as it keeps talking.

use anyhow::Result;
use rosc::OscType;

use crate::config;
use crate::osc::OscLink;

#[derive(Clone)]
pub struct Daw {
    pub link: OscLink,
    cfg: config::LiveTrax,
}

impl Daw {
    pub fn new(link: OscLink, cfg: config::LiveTrax) -> Self {
        Self { link, cfg }
    }

    /// Announce the surface and ask for feedback. Repeated on a timer so the
    /// bridge recovers on its own when the DAW is restarted or a session is
    /// loaded after the bridge came up.
    pub async fn set_surface(&self) -> Result<()> {
        self.link
            .send(
                "/set_surface",
                vec![
                    OscType::Int(self.cfg.bank_size as i32),
                    OscType::Int(self.cfg.strip_types as i32),
                    OscType::Int(self.cfg.feedback as i32),
                    OscType::Int(self.cfg.gain_mode as i32),
                ],
            )
            .await
    }

    pub async fn request_strip_list(&self) -> Result<()> {
        self.link.send("/strip/list", vec![]).await
    }

    pub async fn play(&self) -> Result<()> {
        self.link.send("/transport_play", vec![OscType::Int(1)]).await
    }

    pub async fn stop(&self) -> Result<()> {
        self.link.send("/transport_stop", vec![OscType::Int(1)]).await
    }

    pub async fn toggle_roll(&self) -> Result<()> {
        self.link.send("/toggle_roll", vec![OscType::Int(1)]).await
    }

    pub async fn rec_enable_toggle(&self) -> Result<()> {
        self.link.send("/rec_enable_toggle", vec![OscType::Int(1)]).await
    }

    pub async fn goto_start(&self) -> Result<()> {
        self.link.send("/goto_start", vec![OscType::Int(1)]).await
    }

    pub async fn goto_end(&self) -> Result<()> {
        self.link.send("/goto_end", vec![OscType::Int(1)]).await
    }

    pub async fn next_marker(&self) -> Result<()> {
        self.link.send("/next_marker", vec![OscType::Int(1)]).await
    }

    pub async fn prev_marker(&self) -> Result<()> {
        self.link.send("/prev_marker", vec![OscType::Int(1)]).await
    }

    pub async fn access_action(&self, action: &str) -> Result<()> {
        self.link
            .send("/access_action", vec![OscType::String(action.to_string())])
            .await
    }

    /// Locate the playhead. Positions beyond 32 bits are sent as int64, which
    /// Ardour-family OSC accepts.
    pub async fn locate(&self, samples: i64, roll: bool) -> Result<()> {
        let pos = if samples <= i32::MAX as i64 && samples >= 0 {
            OscType::Int(samples as i32)
        } else {
            OscType::Long(samples)
        };
        self.link
            .send("/locate", vec![pos, OscType::Int(roll as i32)])
            .await
    }

    pub async fn add_marker(&self, name: Option<&str>) -> Result<()> {
        let args = match (name, self.cfg.add_marker_takes_name) {
            (Some(n), true) => vec![OscType::String(n.to_string())],
            _ => vec![OscType::Int(1)],
        };
        self.link.send(self.cfg.add_marker_address.clone(), args).await
    }

    pub async fn rec_enable_strip(&self, ssid: u32, on: bool) -> Result<()> {
        self.link
            .send(
                "/strip/recenable",
                vec![OscType::Int(ssid as i32), OscType::Int(on as i32)],
            )
            .await
    }

    /// Rename a strip. Two wire forms exist across builds, so the address is a
    /// template: with `{ssid}` the id goes in the path, without it the id is
    /// sent as the first argument.
    pub async fn rename_strip(&self, ssid: u32, name: &str) -> Result<()> {
        let tpl = &self.cfg.rename_address;
        if tpl.contains("{ssid}") {
            let addr = crate::osc::template(tpl, &[("ssid", ssid.to_string())]);
            self.link.send(addr, vec![OscType::String(name.to_string())]).await
        } else {
            self.link
                .send(
                    tpl.clone(),
                    vec![OscType::Int(ssid as i32), OscType::String(name.to_string())],
                )
                .await
        }
    }
}
