// Copyright (C) 2019-2021 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io;
use std::pin::Pin;
use std::str::from_utf8;
use std::task::Poll as StdPoll;
use std::time::Duration;

use futures::future::ready;
use futures::future::select;
use futures::future::Either;
use futures::pin_mut;
use futures::stream::unfold;
use futures::task::Context;
use futures::task::Poll;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;

use tokio::time::interval;
use tokio::time::Interval;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;

use tracing::debug;
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


/// Handle a single message from the stream.
async fn handle_msg<S>(
  result: Option<Result<WebSocketMessage, WebSocketError>>,
  stream: &mut S,
) -> Result<(Option<Vec<u8>>, bool), WebSocketError>
where
  S: Sink<WebSocketMessage, Error = WebSocketError> + Unpin,
{
  let result = result.ok_or(WebSocketError::AlreadyClosed)?;
  let msg = result?;

  trace!(recv_msg = debug(&msg));

  match msg {
    WebSocketMessage::Close(_) => Ok((None, true)),
    WebSocketMessage::Text(txt) => {
      debug!(text = display(&txt));
      Ok((Some(txt.into_bytes()), false))
    },
    WebSocketMessage::Binary(dat) => {
      match from_utf8(&dat) {
        Ok(s) => debug!(data = display(&s)),
        Err(b) => debug!(data = display(&b)),
      }
      Ok((Some(dat), false))
    },
    WebSocketMessage::Ping(dat) => {
      let msg = WebSocketMessage::Pong(dat);
      trace!(send_msg = debug(&msg));
      // TODO: We should probably spawn a task here.
      stream.send(msg).await?;
      Ok((None, false))
    },
    WebSocketMessage::Pong(_) => Ok((None, false)),
  }
}

async fn stream_impl<S>(
  stream: S,
  ping_interval: Duration,
) -> impl Stream<Item = Result<Vec<u8>, WebSocketError>>
where
  S: Sink<WebSocketMessage, Error = WebSocketError>,
  S: Stream<Item = Result<WebSocketMessage, WebSocketError>> + Unpin,
{
  let mut ping = Ping::NotNeeded;
  let pinger = interval(ping_interval);
  let (sink, stream) = stream.split();

  unfold((false, (stream, sink, pinger)), move |(closed, (mut stream, mut sink, mut pinger))| {
    async move {
      if closed {
        None
      } else {
        // Note that we could use `futures::stream::select` over
        // manually "polling" each stream, but that has the downside
        // that we cannot bail out quickly if the websocket stream
        // gets exhausted.
        let mut next_msg = StreamExt::next(&mut stream);

        let (result, closed) = loop {
          let next_ping = pinger.tick();
          pin_mut!(next_ping);

          let either = select(next_msg, next_ping).await;
          match either {
            #[allow(unused_assignments)]
            Either::Left((result, _next)) => {
              // We just got a message from the server. Whatever it was,
              // it means our connection is still standing, so no need
              // to ask for a ping.
              ping = Ping::NotNeeded;

              let result = handle_msg(result, &mut sink).await;
              let closed = result.as_ref().map(|(_, closed)| *closed).unwrap_or(false);
              // Note that because we do nothing with `_next` we may
              // actually drop a ping. But we do not consider that
              // critical.
              break (result, closed)
            },
            Either::Right((_ping, next)) => {
              ping = match ping {
                Ping::NotNeeded => Ping::Needed,
                Ping::Needed => {
                  let msg = WebSocketMessage::Ping(Vec::new());
                  trace!(send_msg = debug(&msg));

                  let result = sink.send(msg).await;
                  if let Err(err) = result {
                    break (Err(err), false)
                  }
                  Ping::Pending
                },
                Ping::Pending => {
                  let err = WebSocketError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    "server failed to respond to pings",
                  ));
                  break (Err(err), true)
                },
              };

              next_msg = next;
            },
          }
        };

        Some((result, (closed, (stream, sink, pinger))))
      }
    }
  })
  .try_filter_map(|(res, _)| ready(Ok(res)))
}

/// Create a stream of higher level primitives out of a client, honoring
/// and filtering websocket control messages such as `Ping` and `Close`.
pub async fn stream<S>(stream: S) -> impl Stream<Item = Result<Vec<u8>, WebSocketError>>
where
  S: Sink<WebSocketMessage, Error = WebSocketError>,
  S: Stream<Item = Result<WebSocketMessage, WebSocketError>> + Unpin,
{
  stream_impl(stream, Duration::from_secs(30)).await
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

        if let Some(message) = message {
          if let Err(err) = Pin::new(&mut sink).start_send(message) {
            return Poll::Ready(Err(err))
          }
          *self = Self::Flush;
        }
        Poll::Ready(Ok(()))
      },
      Self::Flush => {
        *self = Self::Unused;
        Pin::new(&mut sink).poll_flush(ctx)
      },
    }
  }
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
            // If anything caused our ping state to change to
            // `NotNeeded` (from the last time we set it to `Needed`)
            // then just change it back to `Needed`.
            Ping::Needed
          },
          Ping::Needed => {
            // The ping state is still `Needed`, which is what we set it
            // to at the last interval. We need to make sure to actually
            // send a ping over the wire now to check whether our
            // connection is still alive.
            let message = WebSocketMessage::Ping(Vec::new());
            // TODO: What happens if our previous ping has not been sent
            //       at this point?
            this.ping = SendMessageState::Pending(Some(message));
            if let Poll::Ready(Err(err)) = this.ping.poll(Pin::new(&mut this.inner), ctx) {
              return Poll::Ready(Some(Err(err)))
            }
            Ping::Pending
          },
          Ping::Pending => {
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
          this.ping_state = Ping::NotNeeded;

          match message {
            WebSocketMessage::Text(data) => break Poll::Ready(Some(Ok(Message::Text(data)))),
            WebSocketMessage::Binary(data) => break Poll::Ready(Some(Ok(Message::Binary(data)))),
            WebSocketMessage::Ping(data) => {
              // Respond with a pong.
              let message = WebSocketMessage::Pong(data);
              // TODO: What happens if our previous pong has not been sent
              //       at this point?
              this.pong = SendMessageState::Pending(Some(message));

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
    Pin::new(&mut self.inner).start_send(message.into())
  }

  fn poll_flush(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
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

  use test_log::test;

  use tokio::time::sleep;

  use tokio_tungstenite::connect_async;
  use tokio_tungstenite::tungstenite::error::ProtocolError;

  use url::Url;

  use crate::test::mock_server;
  use crate::test::WebSocketStream;


  /// Instantiate a websocket server serving data provided by the
  /// given function, connect to said server, and return the resulting
  /// stream.
  async fn serve_and_connect<F, R>(f: F) -> WebSocketStream
  where
    F: FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let addr = mock_server(f).await;
    let url = Url::parse(&format!("ws://{}", addr.to_string())).unwrap();

    let (s, _) = connect_async(url).await.unwrap();
    s
  }

  async fn mock_stream<F, R>(f: F) -> impl Stream<Item = Result<Vec<u8>, WebSocketError>>
  where
    F: Copy + FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    stream::<_>(serve_and_connect(f).await).await
  }

  #[test(tokio::test)]
  async fn no_messages() {
    async fn test(_stream: WebSocketStream) -> Result<(), WebSocketError> {
      Ok(())
    }

    let err = mock_stream(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap_err();

    match err {
      WebSocketError::Protocol(e) if e == ProtocolError::ResetWithoutClosingHandshake => (),
      e => panic!("received unexpected error: {}", e),
    }
  }

  #[test(tokio::test)]
  async fn direct_close() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Just respond with a Close.
      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    mock_stream(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  #[test(tokio::test)]
  async fn decode_error_errors_do_not_terminate() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream
        .send(WebSocketMessage::Text("1337".to_string()))
        .await?;
      stream
        .send(WebSocketMessage::Binary("42".to_string().into_bytes()))
        .await?;
      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    let stream = mock_stream(test).await;
    let messages = StreamExt::collect::<Vec<_>>(stream).await;

    let mut iter = messages.iter();
    assert_eq!(
      iter.next().unwrap().as_ref().unwrap(),
      &"1337".to_string().into_bytes(),
    );
    assert_eq!(
      iter.next().unwrap().as_ref().unwrap(),
      &"42".to_string().into_bytes(),
    );
    assert!(iter.next().is_none());
  }

  #[test(tokio::test)]
  async fn ping_pong() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Ping.
      stream.send(WebSocketMessage::Ping(Vec::new())).await?;
      // Expect Pong.
      assert_eq!(
        StreamExt::next(&mut stream).await.unwrap()?,
        WebSocketMessage::Pong(Vec::new()),
      );

      stream.send(WebSocketMessage::Close(None)).await?;
      Ok(())
    }

    mock_stream(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  #[test(tokio::test)]
  async fn no_pongs() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream
        .send(WebSocketMessage::Text("test".to_string()))
        .await?;

      sleep(Duration::from_secs(10)).await;
      Ok(())
    }

    let ping = Duration::from_millis(1);
    let stream = stream_impl::<_>(serve_and_connect(test).await, ping).await;
    let err = stream.try_for_each(|_| ready(Ok(()))).await.unwrap_err();
    assert_eq!(
      err.to_string(),
      "IO error: server failed to respond to pings"
    );
  }

  #[test(tokio::test)]
  async fn no_messages_dropped() {
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

    let ping = Duration::from_millis(10);
    let stream = stream_impl::<_>(serve_and_connect(test).await, ping).await;
    let stream = StreamExt::map(stream, |r| r.unwrap());
    let stream = StreamExt::map(stream, |r| r);
    let messages = StreamExt::collect::<Vec<_>>(stream).await;
    assert_eq!(
      messages,
      vec!["42".to_string().into_bytes(), "43".to_string().into_bytes()]
    );
  }
}
