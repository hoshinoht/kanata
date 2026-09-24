use std::str;

use crate::core::{ErrorKind, GatewayError};

pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_EVENT_BYTES: usize = 128 * 1024;

pub(crate) struct SseRecord {
    pub(crate) data: String,
    pub(crate) event: Option<String>,
}

#[derive(Default)]
struct EventState {
    bytes: usize,
    data: Vec<String>,
    event: Option<String>,
}

pub(crate) struct SseFramer {
    pending: Vec<u8>,
    /// Prefix of `pending` already searched for a newline.
    scanned: usize,
    event: EventState,
    bom_checked: bool,
    allow_named_events: bool,
    max_line_bytes: usize,
    max_event_bytes: usize,
}

impl SseFramer {
    pub(crate) fn new() -> Self {
        Self::configured(false)
    }

    pub(crate) fn new_with_named_events() -> Self {
        Self::configured(true)
    }

    fn configured(allow_named_events: bool) -> Self {
        Self {
            pending: Vec::new(),
            scanned: 0,
            event: EventState::default(),
            bom_checked: false,
            allow_named_events,
            max_line_bytes: MAX_LINE_BYTES,
            max_event_bytes: MAX_EVENT_BYTES,
        }
    }

    /// Overrides per-line and per-event bounds for upstreams with large events.
    pub(crate) fn with_limits(mut self, max_line_bytes: usize, max_event_bytes: usize) -> Self {
        self.max_line_bytes = max_line_bytes;
        self.max_event_bytes = max_event_bytes;
        self
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseRecord>, GatewayError> {
        self.feed_until(bytes, |_| false)
            .map(|(records, _)| records)
    }

    pub(crate) fn feed_until(
        &mut self,
        bytes: &[u8],
        mut stop: impl FnMut(&SseRecord) -> bool,
    ) -> Result<(Vec<SseRecord>, bool), GatewayError> {
        self.pending.extend_from_slice(bytes);
        self.check_bom()?;
        let mut records = Vec::new();
        while let Some(offset) = self.pending[self.scanned..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let newline = self.scanned + offset;
            self.scanned = 0;
            let mut line: Vec<u8> = self.pending.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > self.max_line_bytes {
                return Err(upstream_failure());
            }
            self.event.bytes = self
                .event
                .bytes
                .checked_add(line.len() + 1)
                .ok_or_else(upstream_failure)?;
            if self.event.bytes > self.max_event_bytes {
                return Err(upstream_failure());
            }
            if line.is_empty() {
                if let Some(record) = self.finish_event()? {
                    let should_stop = stop(&record);
                    records.push(record);
                    if should_stop {
                        self.pending.clear();
                        self.scanned = 0;
                        return Ok((records, true));
                    }
                }
            } else {
                self.field(&line)?;
            }
        }
        self.scanned = self.pending.len();
        if self.pending.len() > self.max_line_bytes {
            return Err(upstream_failure());
        }
        Ok((records, false))
    }

    pub(crate) fn finish(&mut self) -> Result<(), GatewayError> {
        if !self.pending.is_empty() || self.event.bytes != 0 {
            return Err(upstream_failure());
        }
        Ok(())
    }

    fn check_bom(&mut self) -> Result<(), GatewayError> {
        if self.bom_checked {
            return Ok(());
        }
        const BOM: &[u8] = b"\xef\xbb\xbf";
        if self.pending.len() < BOM.len() && BOM.starts_with(&self.pending) {
            return Ok(());
        }
        if self.pending.starts_with(BOM) {
            self.pending.drain(..BOM.len());
            self.scanned = 0;
        }
        self.bom_checked = true;
        Ok(())
    }

    fn field(&mut self, line: &[u8]) -> Result<(), GatewayError> {
        if line[0] == b':' {
            return Ok(());
        }
        let line = str::from_utf8(line).map_err(|_| upstream_failure())?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                self.event.data.push(value.to_owned());
            }
            "event" => {
                if value.contains('\0')
                    || (!self.allow_named_events && !matches!(value, "" | "message"))
                {
                    return Err(upstream_failure());
                }
                if self
                    .event
                    .event
                    .as_deref()
                    .is_some_and(|event| event != value)
                {
                    return Err(upstream_failure());
                }
                self.event.event = Some(value.to_owned());
            }
            "id" | "retry" => {}
            _ => return Err(upstream_failure()),
        }
        Ok(())
    }

    fn finish_event(&mut self) -> Result<Option<SseRecord>, GatewayError> {
        let state = std::mem::take(&mut self.event);
        if state.data.is_empty() {
            return Ok(None);
        }
        Ok(Some(SseRecord {
            data: state.data.join("\n"),
            event: state.event.filter(|event| !event.is_empty()),
        }))
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
