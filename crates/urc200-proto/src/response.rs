//! Response parser (§4.6.3 Data Exchange Protocol).
//!
//! The radio answers every command with ACK, NAK, or HT — possibly preceded
//! by data bytes when the command was a status inquiry. Wire rules per Table 9:
//!
//! - Commands with no data reply: ACK or NAK (or HT on the first keypad→remote).
//! - Commands with a data reply (Table 13 inquiries): data bytes first, then
//!   the single-byte terminator.
//! - HT is sent exactly once, as the transition marker from keypad to remote
//!   mode; afterwards the radio uses ACK/NAK.
//!
//! **Terminator wire format (confirmed against a real URC-200 V2, 2026-04-19):**
//! the manual's "ACK/NAK/HT" are the ASCII control-character names, not the
//! 3-letter strings. Actual wire bytes:
//!
//! | Name | Byte |
//! |------|------|
//! | ACK  | 0x06 |
//! | NAK  | 0x15 |
//! | HT   | 0x09 |
//!
//! The parser buffers incoming bytes as `data` until a terminator byte arrives;
//! the buffered bytes (possibly empty) become the data payload of the event.

const MAX_DATA_LEN: usize = 256;

pub const ACK: u8 = 0x06;
pub const NAK: u8 = 0x15;
pub const HT: u8 = 0x09;

/// A decoded response from the radio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Valid command acknowledged; `data` is empty for operation commands
    /// and non-empty for Table 13 inquiries (e.g. `b"A1"` for synth locked).
    Ack { data: Vec<u8> },
    /// Invalid command. Data before a NAK is not meaningful and is returned
    /// only for diagnostics.
    Nak { data: Vec<u8> },
    /// First valid remote command after the radio was in keypad mode. Treat
    /// like ACK for protocol accounting; `data` carries the inquiry payload
    /// if the first command was a Table 13 inquiry.
    Ht { data: Vec<u8> },
}

impl Response {
    pub fn is_ack(&self) -> bool {
        matches!(self, Response::Ack { .. })
    }
    pub fn is_nak(&self) -> bool {
        matches!(self, Response::Nak { .. })
    }
    pub fn is_ht(&self) -> bool {
        matches!(self, Response::Ht { .. })
    }
    pub fn data(&self) -> &[u8] {
        match self {
            Response::Ack { data } | Response::Nak { data } | Response::Ht { data } => data,
        }
    }
}

/// Streaming parser. Feed bytes; the parser returns a `Response` the moment
/// a terminator is recognised. State automatically resets after each event.
pub struct ResponseParser {
    buf: Vec<u8>,
    overflow: bool,
}

impl Default for ResponseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseParser {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(32),
            overflow: false,
        }
    }

    /// Feed one byte. Returns `Some(Response)` the instant a terminator is
    /// recognised; otherwise `None`. Data bytes accumulate until a single
    /// terminator byte (0x06 ACK, 0x15 NAK, or 0x09 HT) arrives.
    pub fn feed(&mut self, byte: u8) -> Option<Response> {
        match byte {
            ACK => {
                let data = std::mem::take(&mut self.buf);
                Some(Response::Ack { data })
            }
            NAK => {
                let data = std::mem::take(&mut self.buf);
                Some(Response::Nak { data })
            }
            HT => {
                // HT may or may not be preceded by data. On the first command
                // after keypad→remote transition, a Table-13 inquiry *does*
                // emit data+HT (confirmed with `?01` on real hardware:
                // "A1" + 0x09). Treat HT like ACK w.r.t. data capture.
                let data = std::mem::take(&mut self.buf);
                Some(Response::Ht { data })
            }
            _ => {
                if self.buf.len() >= MAX_DATA_LEN {
                    self.overflow = true;
                    self.buf.clear();
                    return None;
                }
                self.buf.push(byte);
                None
            }
        }
    }

    /// Feed a slice of bytes. Returns the list of responses recognised in order.
    pub fn feed_slice(&mut self, bytes: &[u8]) -> Vec<Response> {
        let mut out = Vec::new();
        for &b in bytes {
            if let Some(r) = self.feed(b) {
                out.push(r);
            }
        }
        out
    }

    /// True if any input has exceeded `MAX_DATA_LEN` since construction.
    /// The caller should surface a fault; this is diagnostic only.
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Clear any partial accumulation (e.g. after a Z resync).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.overflow = false;
    }

}

/// Simple consecutive-NAK accounting helper. Per §4.6.3: after 3 NAKs for the
/// same command, the controller should declare a fault and surface an error.
#[derive(Debug, Default)]
pub struct NakCounter {
    consecutive: u8,
}

impl NakCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new response. Returns the outcome for the dispatcher.
    pub fn observe(&mut self, r: &Response) -> DispatchOutcome {
        match r {
            Response::Ack { .. } | Response::Ht { .. } => {
                self.consecutive = 0;
                DispatchOutcome::Ok
            }
            Response::Nak { .. } => {
                self.consecutive = self.consecutive.saturating_add(1);
                if self.consecutive >= 3 {
                    DispatchOutcome::Fault
                } else {
                    DispatchOutcome::Retry { after: self.consecutive }
                }
            }
        }
    }

    pub fn consecutive(&self) -> u8 {
        self.consecutive
    }

    pub fn reset(&mut self) {
        self.consecutive = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Command accepted. Continue.
    Ok,
    /// Command rejected (NAK); caller should send a `Z` resync and re-send.
    /// `after` is the number of consecutive NAKs so far (1 or 2).
    Retry { after: u8 },
    /// 3 consecutive NAKs — protocol fault. Caller must surface and stop.
    Fault,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_ack() {
        let mut p = ResponseParser::new();
        let out = p.feed_slice(&[ACK]);
        assert_eq!(out, vec![Response::Ack { data: vec![] }]);
    }

    #[test]
    fn parses_bare_nak() {
        let mut p = ResponseParser::new();
        let out = p.feed_slice(&[NAK]);
        assert_eq!(out, vec![Response::Nak { data: vec![] }]);
    }

    #[test]
    fn parses_bare_ht() {
        let mut p = ResponseParser::new();
        let out = p.feed_slice(&[HT]);
        assert_eq!(out, vec![Response::Ht { data: vec![] }]);
    }

    #[test]
    fn parses_real_hardware_synth_lock_first_call() {
        // Confirmed wire dump from a real URC-200 V2: "?01" -> "A1" + 0x09 (HT)
        // on the first command after keypad→remote transition.
        let mut p = ResponseParser::new();
        let out = p.feed_slice(b"A1\x09");
        assert_eq!(out, vec![Response::Ht { data: b"A1".to_vec() }]);
    }

    #[test]
    fn parses_real_hardware_synth_lock_second_call() {
        // Once in remote mode: "?01" -> "A1" + 0x06 (ACK).
        let mut p = ResponseParser::new();
        let out = p.feed_slice(b"A1\x06");
        assert_eq!(out, vec![Response::Ack { data: b"A1".to_vec() }]);
    }

    #[test]
    fn parses_data_then_ack_for_rssi() {
        // Table 13 ?03: "Nxxx" RSSI 0-255, followed by ACK.
        let mut p = ResponseParser::new();
        let out = p.feed_slice(b"N128\x06");
        assert_eq!(out, vec![Response::Ack { data: b"N128".to_vec() }]);
    }

    #[test]
    fn parses_multiple_responses_in_stream() {
        let mut p = ResponseParser::new();
        // bare ACK, data+ACK, bare NAK
        let out = p.feed_slice(b"\x06A1\x06\x15");
        assert_eq!(
            out,
            vec![
                Response::Ack { data: vec![] },
                Response::Ack { data: b"A1".to_vec() },
                Response::Nak { data: vec![] },
            ]
        );
    }

    #[test]
    fn single_byte_feed_produces_event_on_terminator() {
        let mut p = ResponseParser::new();
        assert!(p.feed(b'A').is_none());
        assert!(p.feed(b'1').is_none());
        let ev = p.feed(ACK).unwrap();
        assert_eq!(ev, Response::Ack { data: b"A1".to_vec() });
    }

    #[test]
    fn reset_clears_partial_buffer() {
        let mut p = ResponseParser::new();
        p.feed_slice(b"PARTIAL");
        p.reset();
        let out = p.feed_slice(&[ACK]);
        assert_eq!(out, vec![Response::Ack { data: vec![] }]);
    }

    #[test]
    fn overflow_flag_latches_on_long_junk() {
        let mut p = ResponseParser::new();
        let junk = vec![b'X'; MAX_DATA_LEN + 10];
        let out = p.feed_slice(&junk);
        assert!(out.is_empty());
        assert!(p.overflowed());
    }

    #[test]
    fn nak_counter_ok_on_ack() {
        let mut c = NakCounter::new();
        assert_eq!(
            c.observe(&Response::Ack { data: vec![] }),
            DispatchOutcome::Ok
        );
    }

    #[test]
    fn nak_counter_retries_once_then_twice() {
        let mut c = NakCounter::new();
        assert_eq!(
            c.observe(&Response::Nak { data: vec![] }),
            DispatchOutcome::Retry { after: 1 }
        );
        assert_eq!(
            c.observe(&Response::Nak { data: vec![] }),
            DispatchOutcome::Retry { after: 2 }
        );
    }

    #[test]
    fn nak_counter_faults_on_third_nak() {
        let mut c = NakCounter::new();
        c.observe(&Response::Nak { data: vec![] });
        c.observe(&Response::Nak { data: vec![] });
        assert_eq!(
            c.observe(&Response::Nak { data: vec![] }),
            DispatchOutcome::Fault
        );
    }

    #[test]
    fn nak_counter_resets_on_ack_between_naks() {
        let mut c = NakCounter::new();
        c.observe(&Response::Nak { data: vec![] });
        c.observe(&Response::Nak { data: vec![] });
        c.observe(&Response::Ack { data: vec![] });
        assert_eq!(c.consecutive(), 0);
        assert_eq!(
            c.observe(&Response::Nak { data: vec![] }),
            DispatchOutcome::Retry { after: 1 }
        );
    }

    #[test]
    fn nak_counter_resets_on_ht() {
        let mut c = NakCounter::new();
        c.observe(&Response::Nak { data: vec![] });
        c.observe(&Response::Ht { data: vec![] });
        assert_eq!(c.consecutive(), 0);
    }
}
