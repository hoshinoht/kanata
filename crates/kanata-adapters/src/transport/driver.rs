use std::{pin::Pin, task::Poll};

use futures_util::future::poll_fn;
use http::{Request, Response};
use hyper::{body::Incoming, client::conn::http1};

use crate::core::GatewayError;

use super::{encoded::EncodedBody, types};

pub(super) struct RequestDriver<T>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    sender: http1::SendRequest<EncodedBody>,
    connection: Option<http1::Connection<T, EncodedBody>>,
    request: Request<EncodedBody>,
}

impl<T> RequestDriver<T>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    pub(super) fn new(
        sender: http1::SendRequest<EncodedBody>,
        connection: http1::Connection<T, EncodedBody>,
        request: Request<EncodedBody>,
    ) -> Self {
        Self {
            sender,
            connection: Some(connection),
            request,
        }
    }

    pub(super) async fn run(
        mut self,
    ) -> Result<
        (
            Response<Incoming>,
            Option<http1::Connection<T, EncodedBody>>,
        ),
        GatewayError,
    > {
        let mut send = Box::pin(self.sender.send_request(self.request));
        let response = poll_fn(|cx| {
            if let Some(connection) = self.connection.as_mut() {
                match Pin::new(connection).poll(cx) {
                    Poll::Ready(Ok(())) => {
                        self.connection.take();
                    }
                    Poll::Ready(Err(_)) => {
                        self.connection.take();
                        return Poll::Ready(Err(types::upstream_failure()));
                    }
                    Poll::Pending => {}
                }
            }
            match send.as_mut().poll(cx) {
                Poll::Ready(Ok(response)) => Poll::Ready(Ok(response)),
                Poll::Ready(Err(_)) => Poll::Ready(Err(types::upstream_failure())),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        Ok((response, self.connection.take()))
    }
}
