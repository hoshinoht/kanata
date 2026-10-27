use std::sync::{Arc, Mutex, PoisonError};

#[derive(Default)]
struct State {
    requests: u64,
    bytes: u64,
}

pub(crate) struct Uploads {
    state: Mutex<State>,
    max_requests: u64,
    max_bytes: u64,
}

pub(crate) struct Reservation {
    uploads: Arc<Uploads>,
    bytes: u64,
}

impl Uploads {
    pub(crate) fn new(max_requests: u64, max_bytes: u64) -> Self {
        Self {
            state: Mutex::new(State::default()),
            max_requests,
            max_bytes,
        }
    }

    pub(crate) fn acquire(self: &Arc<Self>, body_limit: usize) -> Option<Reservation> {
        // Reserve raw, decoded, and outbound payload capacity.
        let bytes = u64::try_from(body_limit).ok()?.checked_mul(3)?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.requests >= self.max_requests || bytes > self.max_bytes.saturating_sub(state.bytes)
        {
            return None;
        }
        state.requests += 1;
        state.bytes += bytes;
        Some(Reservation {
            uploads: self.clone(),
            bytes,
        })
    }

    pub(crate) fn snapshot(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (state.requests, state.bytes)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self
            .uploads
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.requests -= 1;
        state.bytes -= self.bytes;
    }
}
