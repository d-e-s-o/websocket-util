// Copyright (C) 2019-2020 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::str::from_utf8;

use futures::future::ready;
use futures::stream::unfold;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;

use serde::de::DeserializeOwned;
use serde_json::from_slice as from_json;
use serde_json::Error as JsonError;

use tracing::debug;
use tracing::trace;

use tungstenite::tungstenite::Error as WebSocketError;
use tungstenite::tungstenite::Message;


#[derive(Debug)]
enum Operation<T> {
  /// A value was decoded.
  Decode(T),
  /// A ping was received and we are about to issue a pong.
  Pong(Vec<u8>),
  /// We received a control message that we just ignore.
  Nop,
  /// The connection is supposed to be close.
  Close,
}

impl<T> Operation<T> {
  fn into_decoded(self) -> Option<T> {
    match self {
      Operation::Decode(dat) => Some(dat),
      _ => None,
    }
  }

  fn is_close(&self) -> bool {
    match self {
      Operation::Close => true,
      _ => false,
    }
  }
}


/// Convert a message into an `Operation`.
fn decode_msg<I>(msg: Message) -> Result<Operation<I>, JsonError>
where
  I: DeserializeOwned,
{
  match msg {
    Message::Close(_) => Ok(Operation::Close),
    Message::Text(txt) => {
      debug!(text = display(&txt));
      // TODO: Strictly speaking we would need to check that the
      //       stream is the expected one.
      let resp = from_json::<I>(txt.as_bytes())?;
      Ok(Operation::Decode(resp))
    },
    Message::Binary(dat) => {
      match from_utf8(&dat) {
        Ok(s) => debug!(data = display(&s)),
        Err(b) => debug!(data = display(&b)),
      }

      let resp = from_json::<I>(dat.as_slice())?;
      Ok(Operation::Decode(resp))
    },
    Message::Ping(dat) => Ok(Operation::Pong(dat)),
    Message::Pong(_) => Ok(Operation::Nop),
  }
}

/// Handle a single message from the stream.
async fn handle_msg<S, I>(stream: &mut S) -> Result<Result<Operation<I>, JsonError>, WebSocketError>
where
  S: Sink<Message, Error = WebSocketError>,
  S: Stream<Item = Result<Message, WebSocketError>> + Unpin,
  I: DeserializeOwned,
{
  // TODO: It is unclear whether a WebSocketError received at this
  //       point could potentially be due to a transient issue.
  let result = stream
    .next()
    .await
    .ok_or_else(|| WebSocketError::Protocol("connection lost unexpectedly".into()))?;
  let msg = result?;

  trace!(msg = debug(&msg));

  let result = decode_msg::<I>(msg);
  match result {
    Ok(Operation::Pong(dat)) => {
      // TODO: We should probably spawn a task here.
      stream.send(Message::Pong(dat)).await?;
      Ok(Ok(Operation::Nop))
    },
    op => Ok(op),
  }
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
  unfold((false, stream), |(closed, mut stream)| {
    async move {
      if closed {
        None
      } else {
        let result = handle_msg(&mut stream).await;
        let closed = match result.as_ref() {
          Ok(Ok(op)) => op.is_close(),
          _ => false,
        };

        Some((result, (closed, stream)))
      }
    }
  })
  .try_filter_map(|res| ready(Ok(res.map(|op| op.into_decoded()).transpose())))
}


#[cfg(test)]
mod tests {
  use super::*;

  use std::future::Future;

  use serde::Deserialize;
  use serde::Serialize;
  use serde_json::to_string as to_json;

  use test_env_log::test;

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

  async fn mock_stream<F, R>(
    f: F,
  ) -> impl Stream<Item = Result<Result<Event, JsonError>, WebSocketError>>
  where
    F: Copy + FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
    R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
  {
    let addr = mock_server(f).await;
    let url = Url::parse(&format!("ws://{}", addr.to_string())).unwrap();

    let (s, _) = connect_async(url).await.unwrap();
    stream::<_, Event>(s).await
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
      assert_eq!(stream.next().await.unwrap()?, Message::Pong(Vec::new()),);

      stream.send(Message::Close(None)).await?;
      Ok(())
    }

    let stream = mock_stream(test).await;
    let _ = stream.try_for_each(|_| ready(Ok(()))).await.unwrap();
  }
}
