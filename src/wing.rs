//! Behringer WING side of the bridge.
//!
//! The WING exposes its parameter tree over OSC on UDP 2223. Reads are done by
//! sending the address with no arguments; writes carry the new value. Every
//! address used here comes from config, because the tree has shifted between
//! firmware revisions — run `wing-livetrax-bridge learn` to confirm yours.

use anyhow::Result;
use rosc::OscType;

use crate::config;
use crate::osc::{self, OscLink};

#[derive(Clone)]
pub struct Wing {
    pub link: OscLink,
    cfg: config::Wing,
}

impl Wing {
    pub fn new(link: OscLink, cfg: config::Wing) -> Self {
        Self { link, cfg }
    }

    /// Renew the "push changes to me" subscription. WING firmware drops
    /// subscribers after a few seconds of silence, so this is called on a timer.
    pub async fn subscribe(&self) -> Result<()> {
        for addr in &self.cfg.subscribe {
            self.link.send(addr.clone(), vec![]).await?;
        }
        Ok(())
    }

    pub fn name_address(&self, ch: u16) -> String {
        osc::template(&self.cfg.name_address, &[("ch", ch.to_string())])
    }

    /// Which channel a name message refers to, if any.
    pub fn channel_of_name_address(&self, addr: &str) -> Option<u16> {
        let ch = osc::parse_template(&self.cfg.name_address, "ch", addr)?;
        (ch >= 1 && ch <= self.cfg.channels as u32).then_some(ch as u16)
    }

    pub async fn query_name(&self, ch: u16) -> Result<()> {
        if !self.cfg.query_with_empty_args {
            return Ok(());
        }
        self.link.send(self.name_address(ch), vec![]).await
    }

    pub async fn query_all_names(&self) -> Result<()> {
        for ch in 1..=self.cfg.channels {
            self.query_name(ch).await?;
        }
        Ok(())
    }

    pub async fn set_name(&self, ch: u16, name: &str) -> Result<()> {
        self.link
            .send(self.name_address(ch), vec![OscType::String(name.to_string())])
            .await
    }

    pub async fn recall_scene(&self, addr: &str, index: i32) -> Result<()> {
        self.link.send(addr.to_string(), vec![OscType::Int(index)]).await
    }

    pub async fn send_raw(&self, addr: &str, args: Vec<OscType>) -> Result<()> {
        self.link.send(addr.to_string(), args).await
    }

    /// Ask the console for everything needed to resolve one output group's
    /// patch: what feeds each output, which input each channel owns, and the
    /// names of the mix objects. Replies arrive asynchronously.
    pub async fn query_patch(
        &self,
        live: &crate::config::LivePatch,
        group: &str,
        channels: u16,
    ) -> Result<usize> {
        let outputs = live.group_sizes.get(group).copied().unwrap_or(48);
        let mut asked = 0;
        for n in 1..=outputs {
            let vars = [("grp", group.to_string()), ("n", n.to_string())];
            self.ask(&osc::template(&live.out_source_group, &vars)).await?;
            self.ask(&osc::template(&live.out_source_index, &vars)).await?;
            asked += 2;
            self.breathe(&mut asked).await;
        }
        for ch in 1..=channels {
            let vars = [("ch", ch.to_string())];
            self.ask(&osc::template(&live.channel_input_group, &vars)).await?;
            self.ask(&osc::template(&live.channel_input_index, &vars)).await?;
            asked += 2;
            self.breathe(&mut asked).await;
        }
        for (_, section, count) in crate::patch::OBJECT_SECTIONS {
            for n in 1..=count {
                let vars = [("sect", section.to_string()), ("n", n.to_string())];
                self.ask(&osc::template(&live.object_name, &vars)).await?;
                asked += 1;
                self.breathe(&mut asked).await;
            }
        }
        Ok(asked)
    }

    /// Ask for the console's own labels on a handful of input sockets.
    pub async fn query_input_names(
        &self,
        live: &crate::config::LivePatch,
        refs: &[(String, u16)],
    ) -> Result<usize> {
        let mut asked = 0;
        for (group, index) in refs {
            let vars = [("grp", group.clone()), ("n", index.to_string())];
            self.ask(&osc::template(&live.input_name, &vars)).await?;
            asked += 1;
            self.breathe(&mut asked).await;
        }
        Ok(asked)
    }

    async fn ask(&self, addr: &str) -> Result<()> {
        self.link.send(addr.to_string(), vec![]).await
    }

    /// Hundreds of queries back to back can outrun the console's input buffer.
    async fn breathe(&self, sent: &mut usize) {
        if *sent % 16 == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
}
