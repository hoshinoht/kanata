use std::io;

use tokio::task::{JoinError, JoinSet};

pub(crate) struct ConnectionSets {
    pub(crate) client: JoinSet<()>,
    pub(crate) client_rejections: JoinSet<()>,
    pub(crate) admin: JoinSet<()>,
}

impl ConnectionSets {
    pub(crate) fn new() -> Self {
        Self {
            client: JoinSet::new(),
            client_rejections: JoinSet::new(),
            admin: JoinSet::new(),
        }
    }

    pub(crate) fn reap_completed(&mut self) -> io::Result<()> {
        reap(&mut self.client)?;
        reap(&mut self.client_rejections)?;
        reap(&mut self.admin)
    }

    pub(crate) async fn abort_and_join(&mut self) {
        self.client.abort_all();
        self.client_rejections.abort_all();
        self.admin.abort_all();
        while self.client.join_next().await.is_some() {}
        while self.client_rejections.join_next().await.is_some() {}
        while self.admin.join_next().await.is_some() {}
    }
}

impl Drop for ConnectionSets {
    fn drop(&mut self) {
        self.client.abort_all();
        self.client_rejections.abort_all();
        self.admin.abort_all();
    }
}

fn reap(set: &mut JoinSet<()>) -> io::Result<()> {
    while let Some(result) = set.try_join_next() {
        result.map_err(|error| join_error(&error))?;
    }
    Ok(())
}

pub(crate) fn join_error(_: &JoinError) -> io::Error {
    io::Error::other("server task failed")
}

pub(crate) fn validate_grace(grace: std::time::Duration) -> io::Result<()> {
    tokio::time::Instant::now()
        .checked_add(grace)
        .map(|_| ())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "shutdown grace is too large"))
}

pub(crate) fn grace_deadline(grace: std::time::Duration) -> io::Result<tokio::time::Instant> {
    tokio::time::Instant::now()
        .checked_add(grace)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "shutdown grace is too large"))
}
