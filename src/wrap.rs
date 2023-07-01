// Copyright (C) 2019-2023 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::str::from_utf8 as str_from_utf8;
use std::task::Poll as StdPoll;
use std::time::Duration;

use futures::task::Context;
use futures::task::Poll;
use futures::Sink;
use futures::SinkExt as _;
use futures::Stream;
use futures::StreamExt as _;

use tokio::time::interval;
use tokio::time::Interval;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;

use tracing::debug;
use tracing::error;
use tracing::field::debug;
use tracing::field::DebugValue;
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


/// A message received over a [`Wrapper`].
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
  fn advance<S>(&mut self, sink: &mut S, ctx: &mut Context<'_>) -> Result<(), S::Error>
  where
    S: Sink<M> + Unpin,
    M: Debug,
  {
    match self {
      Self::Unused => Ok(()),
      Self::Pending(message) => {
        match sink.poll_ready_unpin(ctx) {
          Poll::Pending => return Ok(()),
          Poll::Ready(Ok(())) => (),
          Poll::Ready(Err(err)) => {
            *self = Self::Unused;
            return Err(err)
          },
        }

        let message = message.take();
        *self = Self::Unused;
        debug!(
          channel = debug(sink as *const _),
          send_msg = debug(&message)
        );

        if let Some(message) = message {
          sink.start_send_unpin(message)?;
          *self = Self::Flush;
        }
        Ok(())
      },
      Self::Flush => {
        trace!(channel = debug(sink as *const _), msg = "flushing");
        *self = Self::Unused;
        if let Poll::Ready(Err(err)) = sink.poll_flush_unpin(ctx) {
          Err(err)
        } else {
          Ok(())
        }
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


/// A type for displaying debug information for a WebSocket message.
struct DebugMessage<'m> {
  message: &'m WebSocketMessage,
}

impl<'m> Debug for DebugMessage<'m> {
  fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
    match self.message {
      // We could consider also attempting to decode the data passed to
      // Pings/Pongs and Close messages.
      WebSocketMessage::Binary(data) => {
        if let Ok(s) = str_from_utf8(data) {
          f.debug_tuple("Binary").field(&s).finish()
        } else {
          Debug::fmt(self.message, f)
        }
      },
      _ => Debug::fmt(self.message, f),
    }
  }
}

/// Emit a debug representation of a WebSocket message that takes care
/// of converting binary messages to string.
fn debug_message(message: &WebSocketMessage) -> DebugValue<DebugMessage<'_>> {
  debug(DebugMessage { message })
}


/// An internally used type encapsulating the logic of sending pings at
/// regular intervals (if needed).
#[derive(Debug)]
struct Pinger {
  /// The state we maintain for sending pings.
  ping: SendMessageState<WebSocketMessage>,
  /// An object keeping track of when we should be sending the next
  /// ping to the server.
  next_ping: Interval,
  /// State helping us keep track of pings that we want to send to the
  /// server.
  ping_state: Ping,
}

impl Pinger {
  /// Create a new `Pinger` object for sending pings spaced by
  /// `ping_interval`.
  fn new(ping_interval: Duration) -> Self {
    Self {
      ping: SendMessageState::Unused,
      next_ping: interval(ping_interval),
      ping_state: Ping::NotNeeded,
    }
  }

  /// Attempt to advance the ping state by one step.
  #[allow(clippy::result_large_err)]
  fn advance<S>(&mut self, sink: &mut S, ctx: &mut Context<'_>) -> Result<(), S::Error>
  where
    S: Sink<WebSocketMessage, Error = WebSocketError> + Unpin,
  {
    self.ping.advance(sink, ctx)?;

    match self.next_ping.poll_tick(ctx) {
      StdPoll::Ready(_) => {
        // When using the `poll_tick` API, we are on the hook for
        // resetting the interval to be woken up again when it passed.
        let () = self.next_ping.reset();

        // We are due sending a ping according to the user's specified
        // ping interval. Check the existing ping state to decide what
        // to actually do. We may not need to send a ping if we can
        // infer that we had activity in said interval already.
        self.ping_state = match self.ping_state {
          Ping::NotNeeded => {
            trace!(
              channel = debug(sink as *const _),
              msg = "skipping ping due to activity"
            );
            // If anything caused our ping state to change to
            // `NotNeeded` (from the last time we set it to `Needed`)
            // then just change it back to `Needed`.
            Ping::Needed
          },
          Ping::Needed => {
            trace!(channel = debug(sink as *const _), msg = "sending ping");
            // The ping state is still `Needed`, which is what we set it
            // to at the last interval. We need to make sure to actually
            // send a ping over the wire now to check whether our
            // connection is still alive.
            let message = WebSocketMessage::Ping(Vec::new());
            set_message(sink, &mut self.ping, message);

            self.ping.advance(sink, ctx)?;
            Ping::Pending
          },
          Ping::Pending => {
            error!(
              channel = debug(sink as *const _),
              msg = "server failed to respond to pings"
            );
            // We leave it up to clients to decide how to handle missed
            // pings. But in order to not end up in an endless error
            // cycle in case the client does not care, clear our ping
            // state.
            self.ping_state = Ping::Needed;

            let err = WebSocketError::Io(io::Error::new(
              io::ErrorKind::TimedOut,
              "server failed to respond to pings",
            ));
            return Err(err)
          },
        };
        Ok(())
      },
      StdPoll::Pending => Ok(()),
    }
  }

  /// Register the fact that activity occurred over the associated
  /// WebSocket channel, meaning that there is no need to ping the
  /// server this interval.
  fn activity(&mut self) {
    self.ping_state = Ping::NotNeeded;
  }
}


/// A type helping with the construction of [`Wrapper`] objects.
#[derive(Debug)]
pub struct Builder<S> {
  /// The interval at which to send pings. A value of `None` disables
  /// sending of pings.
  ping_interval: Option<Duration>,
  /// Whether or not we send pong replies to ping messages.
  send_pongs: bool,
  /// Phantom data for the WebSocket type.
  _phantom: PhantomData<S>,
}

impl<S> Builder<S> {
  /// Overwrite the default ping interval of 30s with a custom one.
  pub fn set_ping_interval(mut self, interval: Option<Duration>) -> Builder<S> {
    self.ping_interval = interval;
    self
  }

  /// Overwrite the default of sending no pong responses to pings.
  pub fn set_send_pongs(mut self, enable: bool) -> Builder<S> {
    self.send_pongs = enable;
    self
  }

  /// Build the final [`Wrapper`] wrapping the provided WebSocket channel.
  pub fn build(self, channel: S) -> Wrapper<S> {
    Wrapper {
      inner: channel,
      pong: if self.send_pongs {
        Some(SendMessageState::Unused)
      } else {
        None
      },
      ping: self.ping_interval.map(Pinger::new),
    }
  }
}

impl<S> Default for Builder<S> {
  fn default() -> Self {
    Self {
      ping_interval: Some(Duration::from_secs(30)),
      send_pongs: false,
      _phantom: PhantomData,
    }
  }
}


/// A wrapped WebSocket stream that handles responding to pings with
/// pongs, sending of pings to check for liveness of server, and
/// filtering out of WebSocket control messages in the process.
#[derive(Debug)]
#[must_use = "streams do nothing unless polled"]
pub struct Wrapper<S> {
  /// The wrapped stream & sink.
  inner: S,
  /// The state we maintain for sending pongs over our internal sink.
  pong: Option<SendMessageState<WebSocketMessage>>,
  /// The state we maintain for sending pings over our internal sink.
  ping: Option<Pinger>,
}

impl<S> Wrapper<S> {
  /// Create a [`Builder`] for creating a customized `Wrapper`.
  pub fn builder() -> Builder<S> {
    Builder::default()
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
    if let Some(pong) = &mut this.pong {
      if let Err(err) = pong.advance(&mut this.inner, ctx) {
        return Poll::Ready(Some(Err(err)))
      }
    }

    if let Some(ping) = &mut this.ping {
      if let Err(err) = ping.advance(&mut this.inner, ctx) {
        return Poll::Ready(Some(Err(err)))
      }
    }

    loop {
      match this.inner.poll_next_unpin(ctx) {
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
            recv_msg = debug_message(&message)
          );
          let () = this.ping.as_mut().map(Pinger::activity).unwrap_or(());

          match message {
            WebSocketMessage::Text(data) => break Poll::Ready(Some(Ok(Message::Text(data)))),
            WebSocketMessage::Binary(data) => break Poll::Ready(Some(Ok(Message::Binary(data)))),
            WebSocketMessage::Ping(data) => {
              if let Some(pong) = &mut this.pong {
                // Respond with a pong.
                let message = WebSocketMessage::Pong(data);
                set_message(&this.inner, pong, message);

                if let Err(err) = pong.advance(&mut this.inner, ctx) {
                  return Poll::Ready(Some(Err(err)))
                }
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
            WebSocketMessage::Frame(_) => {
              // We should never receive such a value while reading
              // messages.
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
    self.inner.poll_ready_unpin(ctx)
  }

  fn start_send(mut self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
    let message = message.into();
    debug!(
      channel = debug(&self.inner as *const _),
      send_msg = debug_message(&message)
    );
    self.inner.start_send_unpin(message)
  }

  fn poll_flush(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    trace!(channel = debug(&self.inner as *const _), msg = "flushing");
    self.inner.poll_flush_unpin(ctx)
  }

  fn poll_close(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    self.inner.poll_close_unpin(ctx)
  }
}


#[cfg(test)]
mod tests {
  use super::*;

  use std::future::Future;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;
  use std::sync::Arc;

  use futures::future::ready;
  use futures::TryStreamExt as _;

  use rand::seq::IteratorRandom as _;
  use rand::thread_rng;
  use rand::Rng as _;

  use test_log::test;

  use tokio::time::pause;
  use tokio::time::sleep;
  use tokio::time::timeout;

  use tokio_tungstenite::connect_async;
  use tokio_tungstenite::tungstenite::error::ProtocolError;

  use url::Url;

  use crate::test::mock_server;
  use crate::test::WebSocketStream;


  /// Check that we can show proper debug representation of WebSocket
  /// messages.
  #[test]
  fn debug_websocket_message() {
    let message = WebSocketMessage::Binary(b"this is a test".to_vec());
    let expected = r#"Binary("this is a test")"#;
    assert_eq!(format!("{:?}", debug_message(&message)), expected);

    // Also try with some invalid Unicode.
    let message = WebSocketMessage::Binary([0xf0, 0x90, 0x80].to_vec());
    let expected = r#"Binary([240, 144, 128])"#;
    assert_eq!(format!("{:?}", debug_message(&message)), expected);

    let message = WebSocketMessage::Ping(Vec::new());
    let expected = r#"Ping([])"#;
    assert_eq!(format!("{:?}", debug_message(&message)), expected);
  }


  /// Instantiate a WebSocket server serving data provided by the
  /// given function, connect to said server, and return the resulting
  /// wrapped stream.
  async fn serve_and_connect_with_builder<F, R>(
    builder: Builder<WebSocketStream>,
    f: F,
  ) -> Wrapper<WebSocketStream>
  where
    F: FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let addr = mock_server(f).await;
    let url = Url::parse(&format!("ws://{}", addr)).unwrap();

    let (stream, _) = connect_async(url).await.unwrap();
    builder.build(stream)
  }

  /// Create a WebSocket server and connect to it, similar to
  /// `serve_and_connect_with_builder`, but use a pre-defined builder
  /// with a 10ms ping interval.
  async fn serve_and_connect<F, R>(f: F) -> Wrapper<WebSocketStream>
  where
    F: FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let ping = Some(Duration::from_millis(10));
    let builder = Wrapper::builder().set_ping_interval(ping);
    serve_and_connect_with_builder(builder, f).await
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

  /// Verify that ping requests are acknowledged by pongs by the
  /// underlying server.
  #[test(tokio::test)]
  async fn ping_pong() {
    async fn test(stream: WebSocketStream) -> Result<(), WebSocketError> {
      let mut stream = stream.fuse();

      // Ping.
      stream.send(WebSocketMessage::Ping(Vec::new())).await?;
      // Expect Pong.
      assert_eq!(
        stream.next().await.unwrap()?,
        WebSocketMessage::Pong(Vec::new()),
      );

      let future = stream.select_next_some();
      assert!(timeout(Duration::from_millis(20), future).await.is_err());

      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    let builder = Wrapper::builder().set_ping_interval(None);
    serve_and_connect_with_builder(builder, test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  /// Verify that ping requests are acknowledged by pongs by the
  /// underlying server and by our `Wrapper` (if the feature is
  /// enabled).
  #[test(tokio::test)]
  async fn ping_pong_2() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Ping.
      stream.send(WebSocketMessage::Ping(Vec::new())).await?;
      // Expect two Pong messages, one from the underlying server and
      // another from our `Wrapper`.
      assert_eq!(
        stream.next().await.unwrap()?,
        WebSocketMessage::Pong(Vec::new()),
      );
      assert_eq!(
        stream.next().await.unwrap()?,
        WebSocketMessage::Pong(Vec::new()),
      );

      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    let builder = Wrapper::builder().set_send_pongs(true);
    serve_and_connect_with_builder(builder, test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  /// Verify that pings are being sent by our `Wrapper`.
  #[test(tokio::test)]
  async fn pings_are_sent() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Check that we receive Ping messages. The server will take care
      // of responding to them behind our back already, so there is
      // nothing else for us to be done.
      for _ in 0..2 {
        assert!(matches!(
          stream.next().await.unwrap()?,
          WebSocketMessage::Ping(_)
        ));
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

  /// Make sure that we do not get any ping bursts or erroneous ping
  /// timeouts when polling is delayed.
  #[test(tokio::test)]
  async fn no_ping_bursts() {
    let counter = Arc::new(AtomicUsize::new(0));
    let clone = counter.clone();

    let test = |stream: WebSocketStream| async move {
      let mut stream = stream.fuse();

      loop {
        let msg = stream.next().await.unwrap().unwrap();
        if let WebSocketMessage::Ping(_) = msg {
          let _ = clone.fetch_add(1, Ordering::Relaxed);

          // We don't need to respond with a `Pong` here as that's
          // already done by the underlying infrastructure.
        } else {
          panic!("received unexpected message: {msg:?}")
        }
      }
    };

    // Pause time on this thread; this way we have full control over how
    // much time passes while the test runs.
    let () = pause();

    let wrapper = serve_and_connect(test).await;
    // Sleep for 10s (in virtual time). That would have caused roughly
    // 10s / 10ms = 1000 ping intervals to have been missed back when
    // wake ups were accumulated.
    let () = sleep(Duration::from_secs(10)).await;

    let future = wrapper.for_each(|result| {
      assert!(result.is_ok(), "{result:?}");
      ready(())
    });

    // Drain the stream for 15ms, which is enough to trigger a single
    // ping.
    assert!(timeout(Duration::from_millis(15), future).await.is_err());

    // We expect to have seen one ping.
    assert_eq!(counter.load(Ordering::Relaxed), 1);
  }

  /// Verify that no pings are being sent by our `Wrapper` when the
  /// feature is disabled.
  #[test(tokio::test)]
  async fn no_pings_are_sent_when_disabled() {
    async fn test(stream: WebSocketStream) -> Result<(), WebSocketError> {
      let mut stream = stream.fuse();
      let future = stream.select_next_some();
      assert!(timeout(Duration::from_millis(20), future).await.is_err());

      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    let builder = Wrapper::builder().set_ping_interval(None);
    serve_and_connect_with_builder(builder, test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  /// Check that we report an error when the server fails to respond to
  /// pings.
  #[test(tokio::test)]
  async fn no_pong_response() {
    async fn test(_stream: WebSocketStream) -> Result<(), WebSocketError> {
      sleep(Duration::from_secs(10)).await;
      Ok(())
    }

    let mut stream = serve_and_connect(test).await;

    // We should see a timeout error. We test repeatedly to make sure
    // that we continue pinging if errors are ignored by clients.
    for _ in 0..5 {
      let err = stream.next().await.unwrap().unwrap_err();
      match err {
        WebSocketError::Io(err) => {
          assert_eq!(err.kind(), io::ErrorKind::TimedOut);
          assert_eq!(err.to_string(), "server failed to respond to pings");
        },
        _ => panic!("Received unexpected error: {err:?}"),
      }
    }
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
