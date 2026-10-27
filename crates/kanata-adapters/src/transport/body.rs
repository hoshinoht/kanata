use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_core::Stream;
use http_body::{Body, Frame};
use hyper::{body::Incoming, client::conn::http1};

use crate::core::{GatewayError, TimeoutPhase};

use super::{
    encoded::EncodedBody,
    time::{PhaseTimer, Timeouts},
    types::{self, MAX_CHUNK_BYTES, ResponseBody},
};

pub(super) fn bounded_body<T>(
    body: Incoming,
    connection: Option<http1::Connection<T, EncodedBody>>,
    budget: usize,
    timeouts: Timeouts,
) -> ResponseBody
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    Box::pin(BoundedBody {
        body: Some(body),
        connection,
        remaining: budget,
        pending: None,
        first: true,
        timer: None,
        timeouts,
        fused: false,
    })
}

pub(super) fn data_frame(frame: Frame<Bytes>) -> Option<Bytes> {
    match frame.into_data() {
        Ok(data) if !data.is_empty() => Some(data),
        _ => None,
    }
}

struct BoundedBody<T>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    body: Option<Incoming>,
    connection: Option<http1::Connection<T, EncodedBody>>,
    remaining: usize,
    pending: Option<Bytes>,
    first: bool,
    timer: Option<PhaseTimer>,
    timeouts: Timeouts,
    fused: bool,
}

impl<T> Unpin for BoundedBody<T> where T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static
{}

impl<T> Stream for BoundedBody<T>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    type Item = Result<Bytes, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.fused {
            return Poll::Ready(None);
        }
        if let Some(mut bytes) = this.pending.take() {
            let take = bytes.len().min(MAX_CHUNK_BYTES);
            let output = bytes.split_to(take);
            if !bytes.is_empty() {
                this.pending = Some(bytes);
            }
            return Poll::Ready(Some(Ok(output)));
        }

        let phase = if this.first {
            TimeoutPhase::FirstByte
        } else {
            TimeoutPhase::Idle
        };
        let duration = if this.first {
            this.timeouts.first_byte
        } else {
            this.timeouts.idle
        };
        if this.timer.is_none() {
            match PhaseTimer::from_now(duration, phase) {
                Ok(timer) => this.timer = Some(timer),
                Err(error) => return this.finish_error(error),
            }
        }
        if this.timer_expired(cx) {
            return this.finish_error(types::timeout(phase));
        }

        let mut connection_polled = false;
        loop {
            if this.timer_expired_without_wake() {
                return this.finish_error(types::timeout(phase));
            }
            let Some(body) = this.body.as_mut() else {
                return this.finish_eof();
            };
            match Pin::new(body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if this.timer_expired_without_wake() {
                        return this.finish_error(types::timeout(phase));
                    }
                    match data_frame(frame) {
                        None => continue,
                        Some(data) => {
                            if data.len() > this.remaining {
                                return this.finish_error(types::upstream_failure());
                            }
                            this.remaining -= data.len();
                            let mut data = data;
                            let output = data.split_to(data.len().min(MAX_CHUNK_BYTES));
                            if !data.is_empty() {
                                this.pending = Some(data);
                            }
                            if this.timer_expired_without_wake() {
                                return this.finish_error(types::timeout(phase));
                            }
                            this.first = false;
                            this.timer = None;
                            return Poll::Ready(Some(Ok(output)));
                        }
                    }
                }
                Poll::Ready(Some(Err(_))) => {
                    if this.timer_expired_without_wake() {
                        return this.finish_error(types::timeout(phase));
                    }
                    return this.finish_error(types::upstream_failure());
                }
                Poll::Ready(None) => {
                    if this.timer_expired_without_wake() {
                        return this.finish_error(types::timeout(phase));
                    }
                    return this.finish_eof();
                }
                Poll::Pending => {}
            }

            if connection_polled {
                return Poll::Pending;
            }
            connection_polled = true;
            if let Some(connection) = this.connection.as_mut() {
                match Pin::new(connection).poll(cx) {
                    Poll::Ready(Ok(())) => {
                        this.connection.take();
                    }
                    Poll::Ready(Err(_)) => {
                        this.connection.take();
                        return this.finish_error(types::upstream_failure());
                    }
                    Poll::Pending => {}
                }
            }
            if this.timer_expired_without_wake() {
                return this.finish_error(types::timeout(phase));
            }
        }
    }
}

impl<T> BoundedBody<T>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    fn timer_expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.timer
            .as_mut()
            .is_some_and(|timer| timer.poll_expired(cx))
    }

    fn timer_expired_without_wake(&self) -> bool {
        self.timer.as_ref().is_some_and(PhaseTimer::expired)
    }

    fn finish_error(&mut self, error: GatewayError) -> Poll<Option<Result<Bytes, GatewayError>>> {
        self.body.take();
        self.connection.take();
        self.pending.take();
        self.timer.take();
        self.fused = true;
        Poll::Ready(Some(Err(error)))
    }

    fn finish_eof(&mut self) -> Poll<Option<Result<Bytes, GatewayError>>> {
        self.body.take();
        self.connection.take();
        self.pending.take();
        self.timer.take();
        self.fused = true;
        Poll::Ready(None)
    }
}
