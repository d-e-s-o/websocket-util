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

use tokio::stream::StreamExt as TokioStreamExt;
use tokio::time::interval;

use tracing::debug;
use tracing::trace;

use tungstenite::tungstenite::Error as WebSocketError;
use tungstenite::tungstenite::Message;


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
  result: Option<Result<Message, WebSocketError>>,
  stream: &mut S,
) -> Result<(Option<Vec<u8>>, bool), WebSocketError>
where
  S: Sink<Message, Error = WebSocketError> + Unpin,
{
  let result =
    result.ok_or_else(|| WebSocketError::Protocol("connection lost unexpectedly".into()))?;
  let msg = result?;

  trace!(recv_msg = debug(&msg));

  match msg {
    Message::Close(_) => Ok((None, true)),
    Message::Text(txt) => {
      debug!(text = display(&txt));
      Ok((Some(txt.into_bytes()), false))
    },
    Message::Binary(dat) => {
      match from_utf8(&dat) {
        Ok(s) => debug!(data = display(&s)),
        Err(b) => debug!(data = display(&b)),
      }
      Ok((Some(dat), false))
    },
    Message::Ping(dat) => {
      let msg = Message::Pong(dat);
      trace!(send_msg = debug(&msg));
      // TODO: We should probably spawn a task here.
      stream.send(msg).await?;
      Ok((None, false))
    },
    Message::Pong(_) => Ok((None, false)),
  }
}

async fn stream_impl<S>(
  stream: S,
  ping_interval: Duration,
) -> impl Stream<Item = Result<Vec<u8>, WebSocketError>>
where
  S: Sink<Message, Error = WebSocketError>,
  S: Stream<Item = Result<Message, WebSocketError>> + Unpin,
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
          let next_ping = TokioStreamExt::next(&mut pinger);

          let either = select(next_msg, next_ping).await;
          match either {
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
              debug_assert!(_ping.is_some());

              ping = match ping {
                Ping::NotNeeded => Ping::Needed,
                Ping::Needed => {
                  let msg = Message::Ping(Vec::new());
                  trace!(send_msg = debug(&msg));

                  let result = sink.send(msg).await;
                  if let Err(err) = result {
                    break (Err(err), false)
                  }
                  Ping::Pending
                },
                Ping::Pending => {
                  let err = WebSocketError::Protocol("server failed to respond to pings".into());
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
  S: Sink<Message, Error = WebSocketError>,
  S: Stream<Item = Result<Message, WebSocketError>> + Unpin,
{
  stream_impl(stream, Duration::from_secs(30)).await
}


#[cfg(test)]
mod tests {
  use super::*;

  use std::future::Future;

  use test_env_log::test;

  use tokio::time::delay_for;

  use tungstenite::tokio::connect_async;

  use url::Url;

  use crate::test::mock_server;
  use crate::test::WebSocketStream;


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

    mock_stream(test)
      .await
      .try_for_each(|_| ready(Ok(())))
      .await
      .unwrap();
  }

  #[test(tokio::test)]
  async fn decode_error_errors_do_not_terminate() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream.send(Message::Text("1337".to_string())).await?;
      stream
        .send(Message::Binary("42".to_string().into_bytes()))
        .await?;
      stream.send(Message::Close(None)).await?;
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
      stream.send(Message::Ping(Vec::new())).await?;
      // Expect Pong.
      assert_eq!(
        StreamExt::next(&mut stream).await.unwrap()?,
        Message::Pong(Vec::new()),
      );

      stream.send(Message::Close(None)).await?;
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
      stream.send(Message::Text("test".to_string())).await?;

      delay_for(Duration::from_secs(10)).await;
      Ok(())
    }

    let ping = Duration::from_millis(1);
    let stream = stream_impl::<_>(serve_and_connect(test).await, ping).await;
    let err = stream.try_for_each(|_| ready(Ok(()))).await.unwrap_err();
    assert_eq!(
      err.to_string(),
      "WebSocket protocol error: server failed to respond to pings"
    );
  }

  #[test(tokio::test)]
  async fn no_messages_dropped() {
    async fn test(mut stream: WebSocketStream) -> Result<(), WebSocketError> {
      stream.send(Message::Text("42".to_string())).await?;
      stream.send(Message::Pong(Vec::new())).await?;
      stream.send(Message::Text("43".to_string())).await?;
      stream.send(Message::Close(None)).await?;
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
