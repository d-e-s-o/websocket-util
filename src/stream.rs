// Copyright (C) 2019-2020 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::str::from_utf8;
use std::time::Duration;

use futures::future::ready;
use futures::future::select;
use futures::future::Either;
use futures::stream::unfold;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;

use serde::de::DeserializeOwned;
use serde_json::from_slice as from_json;
use serde_json::Error as JsonError;

use tokio::stream::StreamExt as TokioStreamExt;
use tokio::time::interval;

use tracing::debug;
use tracing::trace;

use tungstenite::tungstenite::Error as WebSocketError;
use tungstenite::tungstenite::Message;


#[derive(Debug)]
enum Operation<T> {
  /// A value was decoded.
  Decode(T),
  /// We received a control message that we just ignore.
  Nop,
}

impl<T> Operation<T> {
  fn into_decoded(self) -> Option<T> {
    match self {
      Operation::Decode(dat) => Some(dat),
      _ => None,
    }
  }
}


/// Handle a single message from the stream.
async fn handle_msg<S, I>(
  result: Option<Result<Message, WebSocketError>>,
  stream: &mut S,
) -> Result<(Result<Operation<I>, JsonError>, bool), WebSocketError>
where
  S: Sink<Message, Error = WebSocketError> + Unpin,
  I: DeserializeOwned,
{
  let result =
    result.ok_or_else(|| WebSocketError::Protocol("connection lost unexpectedly".into()))?;
  let msg = result?;

  trace!(msg = debug(&msg));

  match msg {
    Message::Close(_) => Ok((Ok(Operation::Nop), true)),
    Message::Text(txt) => {
      debug!(text = display(&txt));
      match from_json::<I>(txt.as_bytes()) {
        Ok(resp) => Ok((Ok(Operation::Decode(resp)), false)),
        Err(err) => return Ok((Err(err), false)),
      }
    },
    Message::Binary(dat) => {
      match from_utf8(&dat) {
        Ok(s) => debug!(data = display(&s)),
        Err(b) => debug!(data = display(&b)),
      }

      match from_json::<I>(dat.as_slice()) {
        Ok(resp) => Ok((Ok(Operation::Decode(resp)), false)),
        Err(err) => return Ok((Err(err), false)),
      }
    },
    Message::Ping(dat) => {
      // TODO: We should probably spawn a task here.
      stream.send(Message::Pong(dat)).await?;
      Ok((Ok(Operation::Nop), false))
    },
    Message::Pong(_) => Ok((Ok(Operation::Nop), false)),
  }
}

async fn stream_impl<S, I>(
  stream: S,
  ping_interval: Duration,
) -> impl Stream<Item = Result<Result<I, JsonError>, WebSocketError>>
where
  S: Sink<Message, Error = WebSocketError>,
  S: Stream<Item = Result<Message, WebSocketError>> + Unpin,
  I: DeserializeOwned,
{
  let mut pings = 0;
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
          let next_ping = TokioStreamExt::next(&mut pinger);

          let either = select(next_msg, next_ping).await;
          match either {
            Either::Left((result, _next)) => {
              pings = 0;

              let result = handle_msg(result, &mut sink).await;
              let closed = result.as_ref().map(|(_, closed)| *closed).unwrap_or(false);
              // Note that because we do nothing with `_next` we may
              // actually drop a ping. But we do not consider that
              // critical.
              break (result, closed)
            },
            Either::Right((_ping, next)) => {
              debug_assert!(_ping.is_some());

              if pings > 2 {
                let err = WebSocketError::Protocol("server failed to respond to pings".into());
                break (Err(err), true)
              }
              let result = sink.send(Message::Ping(Vec::new())).await;
              if let Err(err) = result {
                break (Err(err), false)
              }

              pings += 1;
              next_msg = next;
            },
          }
        };

        Some((result, (closed, (stream, sink, pinger))))
      }
    }
  })
  .try_filter_map(|(res, _)| ready(Ok(res.map(|op| op.into_decoded()).transpose())))
}

/// Create a stream of higher level primitives out of a client, honoring
/// and filtering websocket control messages such as `Ping` and `Close`.
pub async fn stream<S, I>(
  stream: S,
) -> impl Stream<Item = Result<Result<I, JsonError>, WebSocketError>>
where
  S: Sink<Message, Error = WebSocketError>,
  S: Stream<Item = Result<Message, WebSocketError>> + Unpin,
  I: DeserializeOwned,
{
  stream_impl(stream, Duration::from_secs(30)).await
}


#[cfg(test)]
mod tests {
  use super::*;

  use std::future::Future;

  use serde::Deserialize;
  use serde::Serialize;
  use serde_json::to_string as to_json;

  use test_env_log::test;

  use tokio::time::delay_for;

  use tungstenite::tokio::connect_async;

  use url::Url;

  use crate::test::mock_server;
  use crate::test::WebSocketStream;


  /// A dummy event used for testing.
  #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
  struct Event {
    value: usize,
  }

  impl Event {
    pub fn new(value: usize) -> Self {
      Self { value }
    }
  }

  async fn serve_and_connect<F, R>(
    f: F,
  ) -> impl Stream<Item = Result<Message, WebSocketError>> + Sink<Message, Error = WebSocketError> + Unpin
  where
    F: Copy + FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let addr = mock_server(f).await;
    let url = Url::parse(&format!("ws://{}", addr.to_string())).unwrap();

    let (s, _) = connect_async(url).await.unwrap();
    s
  }

  async fn mock_stream<F, R>(
    f: F,
  ) -> impl Stream<Item = Result<Result<Event, JsonError>, WebSocketError>>
  where
    F: Copy + FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    stream::<_, Event>(serve_and_connect(f).await).await
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
      WebSocketError::Protocol(ref e) if e == "Connection reset without closing handshake" => (),
      e => panic!("received unexpected error: {}", e),
    }
  }

  #[test(tokio::test)]
  async fn direct_close() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Just respond with a Close.
      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let _ = mock_stream(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  #[test(tokio::test)]
  async fn decode_error_errors_do_not_terminate() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream
        .send(Message::Text("{ foobarbaz }".to_string()))
        .await?;
      stream
        .send(Message::Text(to_json(&Event::new(42)).unwrap()))
        .await?;
      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let stream = mock_stream(test).await;
    let events = StreamExt::collect::<Vec<_>>(stream).await;

    let mut iter = events.iter();
    assert!(iter.next().unwrap().as_ref().unwrap().is_err());
    assert_eq!(
      iter.next().unwrap().as_ref().unwrap().as_ref().unwrap(),
      &Event::new(42),
    );
    assert!(iter.next().is_none());
  }

  #[test(tokio::test)]
  async fn ping_pong() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      // Ping.
      stream.send(Message::Ping(Vec::new())).await?;
      // Expect Pong.
      assert_eq!(
        StreamExt::next(&mut stream).await.unwrap()?,
        Message::Pong(Vec::new()),
      );

      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let stream = mock_stream(test).await;
    let _ = stream.try_for_each(|_| ready(Ok(()))).await.unwrap();
  }

  #[test(tokio::test)]
  async fn no_pongs() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream
        .send(Message::Text(to_json(&Event::new(42)).unwrap()))
        .await?;

      delay_for(Duration::from_secs(10)).await;
      Ok(())
    }

    let ping = Duration::from_millis(1);
    let stream = stream_impl::<_, Event>(serve_and_connect(test).await, ping).await;
    let err = stream.try_for_each(|_| ready(Ok(()))).await.unwrap_err();
    assert_eq!(
      err.to_string(),
      "WebSocket protocol error: server failed to respond to pings"
    );
  }

  #[test(tokio::test)]
  async fn no_messages_dropped() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream
        .send(Message::Text(to_json(&Event::new(42)).unwrap()))
        .await?;

      stream.send(Message::Pong(Vec::new())).await?;

      stream
        .send(Message::Text(to_json(&Event::new(43)).unwrap()))
        .await?;

      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let ping = Duration::from_millis(10);
    let stream = stream_impl::<_, Event>(serve_and_connect(test).await, ping).await;
    let stream = StreamExt::map(stream, |r| r.unwrap());
    let stream = StreamExt::map(stream, |r| r.unwrap());
    let events = StreamExt::collect::<Vec<_>>(stream).await;
    assert_eq!(events, vec![Event::new(42), Event::new(43)]);
  }
}
