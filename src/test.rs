// Copyright (C) 2019-2023 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::future::Future;
use std::net::SocketAddr;

use futures::FutureExt;

use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::spawn;

use tokio_tungstenite::accept_async as accept_websocket;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream as WsStream;


/// The WebSocket stream type we use in the server.
pub type WebSocketStream = WsStream<MaybeTlsStream<TcpStream>>;


/// Create a WebSocket server that handles a customizable set of
/// requests and exits.
pub async fn mock_server<F, R>(f: F) -> SocketAddr
where
  F: FnOnce(WebSocketStream) -> R + Send + Sync + 'static,
  R: Future<Output = Result<(), WebSocketError>> + Send + Sync + 'static,
{
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();

  let future = async move {
    listener
      .accept()
      .map(Result::unwrap)
      .then(|(stream, _addr)| accept_websocket(MaybeTlsStream::Plain(stream)))
      .map(Result::unwrap)
      .then(f)
      .await
  };

  let _handle = spawn(future);
  addr
}
