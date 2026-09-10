//! Just enough MIDI to talk to a mixer over a byte stream.
//!
//! The Qu sends a plain MIDI stream over TCP, which means running status,
//! real-time bytes turning up in the middle of other messages, and SysEx that
//! has to be accumulated until its terminator arrives.

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8, velocity: u8 },
    ControlChange { channel: u8, control: u8, value: u8 },
    ProgramChange { channel: u8, program: u8 },
    /// The bytes between F0 and F7, exclusive.
    SysEx(Vec<u8>),
    ActiveSense,
}

#[derive(Default)]
pub struct Parser {
    status: Option<u8>,
    data: Vec<u8>,
    sysex: Option<Vec<u8>>,
}

impl Parser {
    /// Feed one byte. Returns a message when one completes.
    pub fn push(&mut self, byte: u8) -> Option<Message> {
        // System real-time can appear anywhere, even inside a SysEx.
        if byte >= 0xF8 {
            return (byte == 0xFE).then_some(Message::ActiveSense);
        }
        if let Some(buffer) = &mut self.sysex {
            if byte == 0xF7 {
                let payload = self.sysex.take().unwrap_or_default();
                return Some(Message::SysEx(payload));
            }
            if byte < 0x80 {
                buffer.push(byte);
                return None;
            }
            // A status byte inside a SysEx abandons it.
            self.sysex = None;
        }
        if byte == 0xF0 {
            self.sysex = Some(Vec::new());
            self.status = None;
            self.data.clear();
            return None;
        }
        if byte >= 0x80 {
            // System common cancels running status; channel messages set it.
            self.status = (byte < 0xF0).then_some(byte);
            self.data.clear();
            return None;
        }
        let status = self.status?;
        self.data.push(byte);
        let wanted = if matches!(status & 0xF0, 0xC0 | 0xD0) { 1 } else { 2 };
        if self.data.len() < wanted {
            return None;
        }
        let channel = status & 0x0F;
        let message = match status & 0xF0 {
            0x80 => Message::NoteOff { channel, note: self.data[0], velocity: self.data[1] },
            0x90 => {
                // A note on with no velocity is a note off, by convention.
                if self.data[1] == 0 {
                    Message::NoteOff { channel, note: self.data[0], velocity: 0 }
                } else {
                    Message::NoteOn { channel, note: self.data[0], velocity: self.data[1] }
                }
            }
            0xB0 => Message::ControlChange {
                channel,
                control: self.data[0],
                value: self.data[1],
            },
            0xC0 => Message::ProgramChange { channel, program: self.data[0] },
            _ => {
                self.data.clear();
                return None;
            }
        };
        self.data.clear();
        Some(message)
    }

    /// Feed a chunk, collecting whatever completes.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Message> {
        bytes.iter().filter_map(|b| self.push(*b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_stream_with_running_status() {
        let mut parser = Parser::default();
        // Note on, then two more notes relying on running status.
        let messages = parser.feed(&[0x91, 0x3C, 0x7F, 0x3D, 0x7F, 0x3E, 0x00]);
        assert_eq!(
            messages,
            vec![
                Message::NoteOn { channel: 1, note: 0x3C, velocity: 0x7F },
                Message::NoteOn { channel: 1, note: 0x3D, velocity: 0x7F },
                // Velocity zero means off.
                Message::NoteOff { channel: 1, note: 0x3E, velocity: 0 },
            ]
        );
    }

    #[test]
    fn active_sense_passes_through_a_message() {
        let mut parser = Parser::default();
        // The Qu drops FE bytes into the stream wherever it likes.
        let messages = parser.feed(&[0xB0, 0x63, 0xFE, 0x20]);
        assert_eq!(
            messages,
            vec![
                Message::ActiveSense,
                Message::ControlChange { channel: 0, control: 0x63, value: 0x20 },
            ]
        );
    }

    #[test]
    fn collects_sysex_and_ignores_real_time_inside_it() {
        let mut parser = Parser::default();
        let messages = parser.feed(&[
            0xF0, 0x00, 0x00, 0x1A, 0xFE, 0x50, 0x11, 0xF7, 0xC0, 0x05,
        ]);
        assert_eq!(
            messages,
            vec![
                Message::ActiveSense,
                Message::SysEx(vec![0x00, 0x00, 0x1A, 0x50, 0x11]),
                Message::ProgramChange { channel: 0, program: 5 },
            ]
        );
    }

    #[test]
    fn a_truncated_sysex_does_not_swallow_what_follows() {
        let mut parser = Parser::default();
        let messages = parser.feed(&[0xF0, 0x00, 0x01, 0x90, 0x40, 0x7F]);
        assert_eq!(messages, vec![Message::NoteOn { channel: 0, note: 0x40, velocity: 0x7F }]);
    }
}
