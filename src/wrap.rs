// Copyright (C) 2019-2021 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt::Debug;
use std::io;
use std::pin::Pin;
use std::task::Poll as StdPoll;
use std::time::Duration;

use futures::task::Context;
use futures::task::Poll;
use futures::Sink;
use futures::Stream;

use tokio::time::interval;
use tokio::time::Interval;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;

use tracing::debug;
use tracing::error;
use tracing::trace;


/// An enum encapsulating the state machine to handle pings to the
/// server.
#[derive(Clone, Copy, Debug)]
enum Ping {
  /// No ping is needed because we know the connection is still alive.
  NotNeeded,
  /// We haven't heard back from the server in a while and will issue a
  /// ping next.
  Needed,
  /// A ping has been issued and is pending. If we subsequently get
  /// woken up as part of our interval that means no pong was received
  /// and the connection to the server is broken.
  Pending,
}


/// A message received over a `WebSocketStream`.
#[derive(Debug, PartialEq)]
pub enum Message {
  /// A text WebSocket message.
  Text(String),
  /// A binary WebSocket message.
  Binary(Vec<u8>),
}

impl From<Message> for WebSocketMessage {
  fn from(message: Message) -> Self {
    match message {
      Message::Text(data) => WebSocketMessage::Text(data),
      Message::Binary(data) => WebSocketMessage::Binary(data),
    }
  }
}


/// The state we maintain to track the sending of control messages.
#[derive(Debug)]
enum SendMessageState<M> {
  /// The message slot is not in use currently.
  Unused,
  /// A message is pending to be sent.
  ///
  /// Note that the `Option` part is an implementation detail allowing
  /// us to `take` ownership of the message from a `&mut self` context
  /// without requiring that `M: Default` or making similar assumptions.
  Pending(Option<M>),
  /// A message has been sent but not yet flushed.
  Flush,
}

impl<M> SendMessageState<M> {
  /// Attempt to advance the message state by one step.
  fn poll<S>(&mut self, sink: Pin<&mut S>, ctx: &mut Context<'_>) -> Poll<Result<(), S::Error>>
  where
    S: Sink<M> + Unpin,
    M: Debug,
  {
    let mut sink = Pin::get_mut(sink);

    match self {
      Self::Unused => Poll::Ready(Ok(())),
      Self::Pending(message) => {
        match Pin::new(&mut sink).poll_ready(ctx) {
          Poll::Pending => return Poll::Pending,
          Poll::Ready(Ok(())) => (),
          Poll::Ready(Err(err)) => {
            *self = Self::Unused;
            return Poll::Ready(Err(err))
          },
        }

        let message = message.take();
        *self = Self::Unused;
        debug!(
          channel = debug(sink as *const _),
          send_msg = debug(&message)
        );

        if let Some(message) = message {
          if let Err(err) = Pin::new(&mut sink).start_send(message) {
            return Poll::Ready(Err(err))
          }
          *self = Self::Flush;
        }
        Poll::Ready(Ok(()))
      },
      Self::Flush => {
        trace!(channel = debug(sink as *const _), msg = "flushing");
        *self = Self::Unused;
        Pin::new(&mut sink).poll_flush(ctx)
      },
    }
  }

  /// Set a message to be sent
  fn set(&mut self, message: M) {
    *self = Self::Pending(Some(message))
  }
}

/// A helper function for changing some message state that logs
/// issues caused by a message "overrun", i.e., a message not having
/// been sent or flushed yet but an overwrite being requested already.
/// In all likelihood, such errors are caused by a misconfiguration on
/// the user side, i.e., when an extremely small ping interval is set.
fn set_message<S, M>(channel: &S, message_state: &mut SendMessageState<M>, message: M)
where
  M: Debug,
{
  match message_state {
    SendMessageState::Unused => (),
    SendMessageState::Pending(old_message) => {
      debug!(
        channel = debug(channel as *const _),
        send_msg_old = debug(&old_message),
        send_msg_new = debug(&message),
        msg = "message overrun; last message has not been sent"
      );
    },
    SendMessageState::Flush => {
      debug!(
        channel = debug(channel as *const _),
        msg = "message overrun; last message has not been flushed"
      );
    },
  }

  message_state.set(message);
}


/// A wrapped websocket channel that handles responding to pings, sends
/// pings to check for liveness of server, and filters out websocket
/// control messages in the process.
#[derive(Debug)]
#[must_use = "streams do nothing unless polled"]
pub struct Wrapper<S> {
  /// The wrapped stream & sink.
  inner: S,
  /// The state we maintain for sending pongs over our internal sink.
  pong: SendMessageState<WebSocketMessage>,
  /// The state we maintain for sending pings over our internal sink.
  ping: SendMessageState<WebSocketMessage>,
  /// An object keeping track of when we should be sending the next
  /// ping to the server.
  next_ping: Interval,
  /// State helping us keep track of pings that we want to send to the
  /// server.
  ping_state: Ping,
}

impl<S> Wrapper<S> {
  /// Create a `Wrapper` object wrapping the provided stream that uses
  /// the default ping interval (30s).
  pub fn with_default(inner: S) -> Self {
    Self::new(inner, Duration::from_secs(30))
  }

  /// Create a `Wrapper` object wrapping the provided stream.
  pub fn new(inner: S, ping_interval: Duration) -> Self {
    Self {
      inner,
      pong: SendMessageState::Unused,
      ping: SendMessageState::Unused,
      next_ping: interval(ping_interval),
      ping_state: Ping::NotNeeded,
    }
  }
}

impl<S> Stream for Wrapper<S>
where
  S: Sink<WebSocketMessage, Error = WebSocketError>
    + Stream<Item = Result<WebSocketMessage, WebSocketError>>
    + Unpin,
{
  type Item = Result<Message, WebSocketError>;

  fn poll_next(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    let this = Pin::get_mut(self);

    // Start off by trying to advance any sending of pings and pongs.
    if let Poll::Ready(Err(err)) = this.pong.poll(Pin::new(&mut this.inner), ctx) {
      return Poll::Ready(Some(Err(err)))
    }

    if let Poll::Ready(Err(err)) = this.ping.poll(Pin::new(&mut this.inner), ctx) {
      return Poll::Ready(Some(Err(err)))
    }

    // Next check whether we need to send a new ping to the server.
    match this.next_ping.poll_tick(ctx) {
      StdPoll::Ready(_) => {
        // We are due sending a ping according to the user's specified
        // ping interval. Check the existing ping state to decide what
        // to actually do. We may not need to send a ping if we can
        // infer that we had activity in said interval already.
        this.ping_state = match this.ping_state {
          Ping::NotNeeded => {
            trace!(
              channel = debug(&this.inner as *const _),
              msg = "skipping ping due to activity"
            );
            // If anything caused our ping state to change to
            // `NotNeeded` (from the last time we set it to `Needed`)
            // then just change it back to `Needed`.
            Ping::Needed
          },
          Ping::Needed => {
            trace!(
              channel = debug(&this.inner as *const _),
              msg = "sending ping"
            );
            // The ping state is still `Needed`, which is what we set it
            // to at the last interval. We need to make sure to actually
            // send a ping over the wire now to check whether our
            // connection is still alive.
            let message = WebSocketMessage::Ping(Vec::new());
            set_message(&this.inner, &mut this.ping, message);

            if let Poll::Ready(Err(err)) = this.ping.poll(Pin::new(&mut this.inner), ctx) {
              return Poll::Ready(Some(Err(err)))
            }
            Ping::Pending
          },
          Ping::Pending => {
            error!(
              channel = debug(&this.inner as *const _),
              msg = "server failed to respond to pings"
            );
            // We leave it up to clients to decide how to handle missed
            // pings. But in order to not end up in an endless error
            // cycle in case the client does not care, clear our ping
            // state.
            this.ping_state = Ping::Needed;

            let err = WebSocketError::Io(io::Error::new(
              io::ErrorKind::Other,
              "server failed to respond to pings",
            ));
            return Poll::Ready(Some(Err(err)))
          },
        };
      },
      StdPoll::Pending => (),
    }

    loop {
      match Pin::new(&mut this.inner).poll_next(ctx) {
        Poll::Pending => {
          // No new data is available yet. There is nothing to do for us
          // except bubble up this result.
          break Poll::Pending
        },
        Poll::Ready(None) => {
          // The stream is exhausted. Bubble up the result and be done.
          break Poll::Ready(None)
        },
        Poll::Ready(Some(Err(err))) => break Poll::Ready(Some(Err(err))),
        Poll::Ready(Some(Ok(message))) => {
          debug!(
            channel = debug(&this.inner as *const _),
            recv_msg = debug(&message)
          );
          this.ping_state = Ping::NotNeeded;

          match message {
            WebSocketMessage::Text(data) => break Poll::Ready(Some(Ok(Message::Text(data)))),
            WebSocketMessage::Binary(data) => break Poll::Ready(Some(Ok(Message::Binary(data)))),
            WebSocketMessage::Ping(data) => {
              // Respond with a pong.
              let message = WebSocketMessage::Pong(data);
              set_message(&this.inner, &mut this.pong, message);

              if let Poll::Ready(Err(err)) = this.pong.poll(Pin::new(&mut this.inner), ctx) {
                return Poll::Ready(Some(Err(err)))
              }
            },
            WebSocketMessage::Pong(_) => {
              // We don't handle pongs any specifically. We already
              // registered that we received a message above.
            },
            WebSocketMessage::Close(_) => {
              // We just ignore close messages. From our perspective
              // they serve no purpose, because the stream already has a
              // notion of "exhausted" with `Poll::Ready(None)`. The
              // only value-add they provide is the optional close
              // frame, but we don't intend to use it or expose it to
              // clients.
            },
          }
        },
      }
    }
  }
}

impl<S> Sink<Message> for Wrapper<S>
where
  S: Sink<WebSocketMessage, Error = WebSocketError> + Unpin,
{
  type Error = WebSocketError;

  fn poll_ready(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    Pin::new(&mut self.inner).poll_ready(ctx)
  }

  fn start_send(mut self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
    let message = message.into();
    debug!(
      channel = debug(&self.inner as *const _),
      send_msg = debug(&message)
    );
    Pin::new(&mut self.inner).start_send(message)
  }

  fn poll_flush(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    trace!(channel = debug(&self.inner as *const _), msg = "flushing");
    Pin::new(&mut self.inner).poll_flush(ctx)
  }

  fn poll_close(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    Pin::new(&mut self.inner).poll_close(ctx)
  }
}


#[cfg(test)]
mod tests {
  use super::*;

  use std::future::Future;

  use futures::future::ready;
  use futures::SinkExt as _;
  use futures::StreamExt as _;
  use futures::TryStreamExt as _;

  use rand::seq::IteratorRandom as _;
  use rand::thread_rng;
  use rand::Rng as _;

  use test_log::test;

  use tokio::time::sleep;

  use tokio_tungstenite::connect_async;
  use tokio_tungstenite::tungstenite::error::ProtocolError;

  use url::Url;

  use crate::test::mock_server;
  use crate::test::WebSocketStream;


  /// Instantiate a websocket server serving data provided by the
  /// given function, connect to said server, and return the resulting
  /// wrapped stream.
  async fn serve_and_connect<F, R>(f: F) -> Wrapper<WebSocketStream>
  where
    F: FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let addr = mock_server(f).await;
    let url = Url::parse(&format!("ws://{}", addr.to_string())).unwrap();

    let (stream, _) = connect_async(url).await.unwrap();
    let ping = Duration::from_millis(10);
    Wrapper::new(stream, ping)
  }

  /// Check that our `Wrapper` behaves correctly if no messages are sent
  /// at all.
  #[test(tokio::test)]
  async fn no_messages() {
    async fn test(_stream: WebSocketStream) -> Result<(), WebSocketError> {
      Ok(())
    }

    let err = serve_and_connect(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap_err();

    match err {
      WebSocketError::Protocol(e) if e == ProtocolError::ResetWithoutClosingHandshake => (),
      e => panic!("received unexpected error: {}", e),
    }
  }

  /// Check that our `Wrapper` handles a straight close without other
  /// messages correctly.
  #[test(tokio::test)]
  async fn direct_close() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Just respond with a Close.
      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    serve_and_connect(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  /// Verify that ping requests are acknowledged by pongs.
  #[test(tokio::test)]
  async fn ping_pong() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Ping.
      stream.send(WebSocketMessage::Ping(Vec::new())).await?;
      // Expect Pong.
      assert_eq!(
        stream.next().await.unwrap()?,
        WebSocketMessage::Pong(Vec::new()),
      );

      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    serve_and_connect(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  /// Check that we report an error when the server fails to respond to
  /// pings.
  #[test(tokio::test)]
  async fn no_pongs() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream
        .send(WebSocketMessage::Text("test".to_string()))
        .await?;

      sleep(Duration::from_secs(10)).await;
      Ok(())
    }

    let stream = serve_and_connect(test).await;
    let err = stream.try_for_each(|_| ready(Ok(()))).await.unwrap_err();
    assert_eq!(
      err.to_string(),
      "IO error: server failed to respond to pings"
    );
  }

  /// Check that messages sent by the server are transported correctly.
  #[test(tokio::test)]
  async fn send_messages() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream
        .send(WebSocketMessage::Text("42".to_string()))
        .await?;
      stream.send(WebSocketMessage::Pong(Vec::new())).await?;
      stream
        .send(WebSocketMessage::Text("43".to_string()))
        .await?;
      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    let stream = serve_and_connect(test).await;
    let messages = stream.try_collect::<Vec<_>>().await.unwrap();
    assert_eq!(
      messages,
      vec![
        Message::Text("42".to_string()),
        Message::Text("43".to_string())
      ]
    );
  }

  /// Stress test our `Wrapper` type's `Stream` part by sending
  /// excessive amount of messages through it.
  #[test(tokio::test)]
  #[ignore = "stress test; test takes a long time"]
  async fn stress_stream() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      fn random_buf() -> Vec<u8> {
        let len = (0..32).choose(&mut thread_rng()).unwrap();
        let mut vec = Vec::new();
        vec.extend((0..len).map(|_| thread_rng().gen::<u8>()));
        vec
      }

      for _ in 0..50000 {
        let message = match (0..5).choose(&mut thread_rng()).unwrap() {
          0 => WebSocketMessage::Pong(random_buf()),
          // Note that we can't really spam `Ping` messages here. The
          // server may actually send them itself and so if the server
          // sends one, then "we" send one immediately after, we may
          // drop the first pong response on the floor and just send the
          // second one. However, because the payloads do not match the
          // server may conclude that something was amiss and terminate
          // the connection.
          i => {
            if i & 0x1 == 0 {
              let len = (0..32).choose(&mut thread_rng()).unwrap();
              let mut string = String::new();
              string.extend((0..len).map(|_| thread_rng().gen::<char>()));

              WebSocketMessage::Text(string)
            } else {
              WebSocketMessage::Binary(random_buf())
            }
          },
        };

        stream.send(message).await?;
      }

      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    serve_and_connect(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }
}
