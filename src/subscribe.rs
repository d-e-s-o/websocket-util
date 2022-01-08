// Copyright (C) 2021 Daniel Mueller (deso@posteo.net)
// SPDX-License-Identifier: GPL-3.0-or-later

//! A module providing low-level building blocks for controlling a
//! websocket connection with an embedded control channel through an
//! external subscription object.

use std::convert::Infallible;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use futures::channel::oneshot::channel;
use futures::channel::oneshot::Canceled;
use futures::channel::oneshot::Sender;
use futures::select_biased;
use futures::sink::SinkExt as _;
use futures::stream::FusedStream;
use futures::task::Context;
use futures::task::Poll;
use futures::Future;
use futures::FutureExt as _;
use futures::Sink;
use futures::Stream;
use futures::StreamExt as _;


/// An enum for the possible classifications of a message.
#[derive(Debug)]
pub enum Classification<U, C> {
  /// A user message. When classified as such, the message is directly
  /// emitted by the `MessageStream` and the associated `Subscription`
  /// is not informed.
  UserMessage(U),
  /// A control message. Such a message is forwarded to the
  /// `Subscription`. It is never emitted by the `MessageStream`.
  ControlMessage(C),
}


/// A trait allowing our stream and subscription infrastructure to work
/// with messages.
pub trait Message {
  /// A message that is relevant to the user.
  type UserMessage;
  /// An internally used control message.
  type ControlMessage;

  /// Classify a message as a user-relevant message or a control
  /// message.
  fn classify(self) -> Classification<Self::UserMessage, Self::ControlMessage>;

  /// Check whether a user message is considered an error. Erroneous
  /// messages cause any ongoing [`Subscription::send`] or
  /// [`Subscription::read`] requests to result in an error.
  fn is_error(user_message: &Self::UserMessage) -> bool;
}


/// State shared between the message stream and the subscription.
///
/// This state is an optional one-shot channel that is set by the
/// subscription whenever it expects to receive a control message.
type SharedState<M> = Arc<Mutex<Option<Sender<Option<Result<M, ()>>>>>>;


/// A stream of messages that is associated with a [`Subscription`].
#[derive(Debug)]
pub struct MessageStream<S, M>
where
  M: Message,
{
  /// The internally used stream.
  stream: S,
  /// State shared between the message stream and the subscription.
  shared: SharedState<M::ControlMessage>,
}

impl<S, M> MessageStream<S, M>
where
  M: Message,
{
  /// Inform the associated [`Subscription`] about a message, if it is
  /// registered to receive such notifications.
  fn inform_subscription(
    shared: &SharedState<M::ControlMessage>,
    message: Option<Result<M::ControlMessage, ()>>,
  ) {
    let sender = shared
      .lock()
      .map_err(|err| err.into_inner())
      .unwrap_or_else(|err| err)
      .take();

    if let Some(sender) = sender {
      // If the `Subscription` registered a `Sender`, use it to send the
      // provided message. If delivery failed the `Subscription` object
      // has already been dropped. That is fine and we will just ignore
      // the error.
      let _ = sender.send(message);
    }
  }
}

impl<S, M> Stream for MessageStream<S, M>
where
  S: Stream<Item = M> + Unpin,
  M: Message,
{
  type Item = M::UserMessage;

  fn poll_next(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    let shared = self.shared.clone();
    let this = self.get_mut();

    loop {
      let stream = Pin::new(&mut this.stream);

      match stream.poll_next(ctx) {
        Poll::Pending => {
          // No new data is available yet. There is nothing to do for us
          // except bubble up this result.
          break Poll::Pending
        },
        Poll::Ready(None) => {
          // The connection got terminated. We need to convey that to
          // the `Subscription`.
          Self::inform_subscription(&shared, None);
          break Poll::Ready(None)
        },
        Poll::Ready(Some(message)) => {
          match message.classify() {
            Classification::UserMessage(user_message) => {
              if M::is_error(&user_message) {
                // If message classification deduced an error, inform
                // the subscription about that fact (to unblock
                // requests) but yield the actual error via the message
                // stream (note that we cannot assume that errors are
                // clonable and that's why we only indicate *that* an
                // error occurred to the subscription).
                Self::inform_subscription(&shared, Some(Err(())));
              }
              // The `Subscription` is oblivious to user messages,
              // so just return it.
              break Poll::Ready(Some(user_message))
            },
            Classification::ControlMessage(control_message) => {
              // We encountered a control message. Push it to the
              // subscription and then just continue polling.
              // Clients of the message stream do not care about
              // these.
              Self::inform_subscription(&shared, Some(Ok(control_message)));
            },
          }
        },
      }
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    Stream::size_hint(&self.stream)
  }
}

impl<S, M> FusedStream for MessageStream<S, M>
where
  S: FusedStream<Item = M> + Unpin,
  M: Message,
{
  #[inline]
  fn is_terminated(&self) -> bool {
    self.stream.is_terminated()
  }
}


/// A subscription associated with a [`MessageStream`] that allows for
/// sending and receiving control messages over it.
///
/// # Notes
/// - in order for any [`send`][Subscription::send] or
///   [`read`][Subscription::read] operations to resolve, the associated
///   [`MessageStream`] stream needs to be polled; that is necessary
///   because this operation expects a control message response and that
///   control message comes through the regular stream.
#[derive(Debug)]
pub struct Subscription<S, M, I>
where
  M: Message,
{
  /// A sink to which we can send control messages.
  sink: S,
  /// State shared between the subscription and the message stream.
  shared: SharedState<M::ControlMessage>,
  /// Phantom data for our sink's item type, which does not have to be
  /// the actual control message type.
  _phantom: PhantomData<I>,
}

impl<S, M, I> Subscription<S, M, I>
where
  S: Sink<I> + Unpin,
  M: Message,
{
  /// Install a one-shot channel, run a function being passed our sink,
  /// and then wait for a message being received through the channel.
  async fn with_channel<'slf, F, G, E>(
    &'slf mut self,
    f: F,
  ) -> Result<Option<Result<M::ControlMessage, ()>>, E>
  where
    F: FnOnce(&'slf mut S) -> G,
    G: Future<Output = Result<(), E>>,
  {
    // Create a one-shot channel and register it with the message stream
    // via our shared state.
    let (sender, receiver) = channel();
    let _prev = self
      .shared
      .lock()
      .map_err(|err| err.into_inner())
      .unwrap_or_else(|err| err)
      .replace(sender);
    debug_assert!(_prev.is_none());

    if let Err(err) = f(&mut self.sink).await {
      // We are about to exit early, because we failed to send our
      // message over the control channel. Make sure to clean up the
      // shared state that we installed earlier, so that the invariant
      // that we always enter this function with a `None` in `shared` is
      // preserved.
      let _prev = self
        .shared
        .lock()
        .map_err(|err| err.into_inner())
        .unwrap_or_else(|err| err)
        .take();
      debug_assert!(_prev.is_some());
      return Err(err)
    }

    let result = receiver.await;
    // Our `MessageStream` type should make sure to "take" the sender
    // that we put in.
    debug_assert!(self
      .shared
      .lock()
      .map_err(|err| err.into_inner())
      .unwrap_or_else(|err| err)
      .is_none());
    // The only reason for getting back an `Err` here is if the sender
    // got dropped. That should never happen (because we control it),
    // but we map that to a `None`, just in case.
    Ok(Result::<_, Canceled>::unwrap_or(result, None))
  }

  /// Send a message over the internal control channel and wait a
  /// control message response.
  ///
  /// The method returns the following errors:
  /// - `Err(..)` when the sink failed to send an item
  /// - `Ok(None)` when the message stream got closed
  /// - `Ok(Some(Err(())))` when message classification reported an
  ///    error; the actual error still manifests through the message
  ///    stream
  pub async fn send(&mut self, item: I) -> Result<Option<Result<M::ControlMessage, ()>>, S::Error> {
    self
      .with_channel(|sink| async move { sink.send(item).await })
      .await
  }

  /// Wait for a control message to arrive.
  pub async fn read(&mut self) -> Option<Result<M::ControlMessage, ()>> {
    let result = self.with_channel(|_sink| async { Ok(()) }).await;

    // It's fine to unwrap here because we statically guarantee that an
    // error can never occur.
    Result::<_, Infallible>::unwrap(result)
  }
}


/// Wrap a stream and an associated control channel into a connected
/// ([`MessageStream`], [`Subscription`]) pair, in which the
/// subscription can be used to send and receive control messages over
/// the stream.
pub fn subscribe<M, I, St, Si>(
  stream: St,
  control_channel: Si,
) -> (MessageStream<St, M>, Subscription<Si, M, I>)
where
  M: Message,
  St: Stream<Item = M>,
  Si: Sink<I>,
{
  let shared = Arc::new(Mutex::new(None));

  let subscription = Subscription {
    sink: control_channel,
    shared: shared.clone(),
    _phantom: PhantomData,
  };
  let message_stream = MessageStream { stream, shared };

  (message_stream, subscription)
}


/// Helper function to drive a [`Subscription`] related future to
/// completion. The function makes sure to poll the provided stream,
/// which is assumed to be associated with the `Subscription` that the
/// future belongs to, so that control messages can be received. Errors
/// reported by the stream (identified via [`Message::is_error`]) short
/// circuit and fail the operation immediately.
pub async fn drive<M, F, S>(future: F, stream: &mut S) -> Result<F::Output, M::UserMessage>
where
  M: Message,
  F: Future + Unpin,
  S: FusedStream<Item = M::UserMessage> + Unpin,
{
  let mut future = future.fuse();

  'l: loop {
    select_biased! {
      output = future => break 'l Ok(output),
      user_message = stream.next() => {
        if let Some(user_message) = user_message {
          if M::is_error(&user_message) {
            break 'l Err(user_message)
          }
        }
      },
    }
  }
}


#[cfg(test)]
mod tests {
  use super::*;

  use futures::channel::mpsc::channel;
  use futures::stream::iter;

  use test_log::test;


  /// A "dummy" message type used for testing.
  #[derive(Debug)]
  enum MockMessage<T> {
    /// The actual user visible message.
    Value(T),
    /// A "control" message.
    Close(u8),
  }

  impl<T> Message for MockMessage<T> {
    type UserMessage = T;
    type ControlMessage = u8;

    fn classify(self) -> Classification<Self::UserMessage, Self::ControlMessage> {
      match self {
        MockMessage::Value(x) => Classification::UserMessage(x),
        MockMessage::Close(x) => Classification::ControlMessage(x),
      }
    }

    #[inline]
    fn is_error(_user_message: &Self::UserMessage) -> bool {
      // In this implementation there are no errors.
      false
    }
  }


  /// Check that we can send a message through a `Subscription` and
  /// receive back the expected control message response.
  #[test(tokio::test)]
  async fn send_recv() {
    let mut it = iter([
      MockMessage::Value(1u64),
      MockMessage::Value(2u64),
      MockMessage::Value(3u64),
      MockMessage::Close(200),
      MockMessage::Close(201),
      MockMessage::Value(4u64),
    ])
    .map(Ok);

    // Note that for this test the channel's capacity must be greater or
    // equal to the number of messages we list above, otherwise the
    // `send_all` below may just deadlock, because nobody is draining
    // the receiver yet.
    let (mut send, recv) = channel::<MockMessage<u64>>(16);
    let () = send.send_all(&mut it).await.unwrap();

    let (mut message_stream, mut subscription) = subscribe(recv, send);
    let close = subscription.send(MockMessage::Close(42)).boxed_local();
    let message = drive::<MockMessage<u64>, _, _>(close, &mut message_stream)
      .await
      .unwrap()
      .unwrap();

    // We should have received back the first "control" message that we
    // fed into the stream, which has the payload 200.
    assert_eq!(message, Some(Ok(200)));
  }

  /// Check that we can wait for a control message without sending
  /// anything beforehand.
  #[test(tokio::test)]
  async fn read() {
    let mut it = iter([
      MockMessage::Value(1u64),
      MockMessage::Value(2u64),
      MockMessage::Value(3u64),
      MockMessage::Close(200),
      MockMessage::Close(201),
      MockMessage::Value(4u64),
    ])
    .map(Ok);

    let (mut send, recv) = channel::<MockMessage<u64>>(16);
    let () = send.send_all(&mut it).await.unwrap();

    let (mut message_stream, mut subscription) = subscribe(recv, send);
    let close = subscription.read().boxed_local();
    let message = drive::<MockMessage<u64>, _, _>(close, &mut message_stream)
      .await
      .unwrap();

    assert_eq!(message, Some(Ok(200)));
  }

  /// Check that `Subscription::send` behaves correctly if the
  /// associated message stream gets dropped.
  #[test(tokio::test)]
  async fn stream_drop() {
    let (send, recv) = channel::<MockMessage<u64>>(1);

    let (message_stream, mut subscription) = subscribe(recv, send);
    drop(message_stream);

    let result = subscription.send(MockMessage::Close(42)).await;
    assert!(result.is_err());

    // Try another time to make sure that we do not leave any state
    // invariant-violating state behind.
    let result = subscription.send(MockMessage::Close(41)).await;
    assert!(result.is_err());
  }

  /// Check that `Subscription::send` behaves correctly if the
  /// underlying control channel gets closed.
  #[test(tokio::test)]
  async fn control_channel_closed() {
    let (mut send, recv) = channel::<MockMessage<u64>>(1);
    send.close_channel();

    let (_message_stream, mut subscription) = subscribe(recv, send);

    let result = subscription.send(MockMessage::Close(42)).await;
    assert!(result.is_err());

    let result = subscription.send(MockMessage::Close(41)).await;
    assert!(result.is_err());
  }

  /// Check that a `MessageStream` behaves correctly if its associated
  /// `Subscription` has been dropped.
  #[test(tokio::test)]
  async fn stream_processing_with_dropped_subscription() {
    let mut it = iter([
      MockMessage::Value(1u64),
      MockMessage::Close(200),
      MockMessage::Value(4u64),
    ])
    .map(Ok);

    let (mut send, recv) = channel::<MockMessage<u64>>(4);
    let () = send.send_all(&mut it).await.unwrap();

    let (message_stream, subscription) = subscribe(recv, send);
    drop(subscription);

    let vec = message_stream.collect::<Vec<_>>().await;
    assert_eq!(vec, vec![1u64, 4u64]);
  }


  impl<T> Message for Result<Result<MockMessage<T>, String>, u64> {
    type UserMessage = Result<Result<T, String>, u64>;
    type ControlMessage = u8;

    fn classify(self) -> Classification<Self::UserMessage, Self::ControlMessage> {
      match self {
        Ok(Ok(MockMessage::Value(x))) => Classification::UserMessage(Ok(Ok(x))),
        Ok(Ok(MockMessage::Close(x))) => Classification::ControlMessage(x),
        // Inner errors (e.g., JSON errors) are directly reported as
        // errors.
        Ok(Err(err)) => Classification::UserMessage(Ok(Err(err))),
        // We push through outer errors (simulating websocket errors) as
        // user messages.
        Err(err) => Classification::UserMessage(Err(err)),
      }
    }

    fn is_error(user_message: &Self::UserMessage) -> bool {
      // We only report inner errors as errors for the sake of testing.
      user_message
        .as_ref()
        .map(|inner| inner.is_err())
        .unwrap_or(false)
    }
  }


  /// Make sure that event with nested errors sending and receiving type
  /// checks and works.
  #[test(tokio::test)]
  async fn send_recv_with_errors() {
    let mut it = iter([
      Ok(Ok(MockMessage::Value(1u64))),
      Ok(Ok(MockMessage::Value(2u64))),
      Ok(Ok(MockMessage::Value(3u64))),
      Ok(Ok(MockMessage::Close(200))),
      Ok(Ok(MockMessage::Close(201))),
      Ok(Ok(MockMessage::Value(4u64))),
    ])
    .map(Ok);

    let (mut send, recv) = channel::<Result<Result<MockMessage<u64>, String>, u64>>(16);
    let () = send.send_all(&mut it).await.unwrap();

    let (mut message_stream, mut subscription) = subscribe(recv, send);
    let close = subscription
      .send(Ok(Ok(MockMessage::Close(42))))
      .boxed_local();
    let message =
      drive::<Result<Result<MockMessage<u64>, String>, u64>, _, _>(close, &mut message_stream)
        .await
        .unwrap()
        .unwrap();

    // We should have received back the first "control" message that we
    // fed into the stream, which has the payload 200.
    assert_eq!(message, Some(Ok(200)));
  }

  /// Check that inner errors are pushed to the subscription properly.
  #[test(tokio::test)]
  async fn inner_error() {
    let mut it = iter([
      Ok(Ok(MockMessage::Value(1u64))),
      Ok(Err("error".to_string())),
      Ok(Ok(MockMessage::Close(200))),
    ])
    .map(Ok);

    let (mut send, recv) = channel::<Result<Result<MockMessage<u64>, String>, u64>>(16);
    let () = send.send_all(&mut it).await.unwrap();

    let (mut message_stream, mut subscription) = subscribe(recv, send);
    let close = subscription
      .send(Ok(Ok(MockMessage::Close(42))))
      .boxed_local();
    let message =
      drive::<Result<Result<MockMessage<u64>, String>, u64>, _, _>(close, &mut message_stream)
        .await
        .unwrap_err()
        .unwrap();

    assert_eq!(message, Err("error".to_string()));
  }

  /// Check that outer errors are ignored by the subscription.
  #[test(tokio::test)]
  async fn outer_error() {
    let mut it = iter([
      Ok(Ok(MockMessage::Value(1u64))),
      Err(1337),
      Ok(Ok(MockMessage::Close(200))),
    ])
    .map(Ok);

    let (mut send, recv) = channel::<Result<Result<MockMessage<u64>, String>, u64>>(16);
    let () = send.send_all(&mut it).await.unwrap();

    let (mut message_stream, mut subscription) = subscribe(recv, send);
    let close = subscription
      .send(Ok(Ok(MockMessage::Close(42))))
      .boxed_local();
    let message =
      drive::<Result<Result<MockMessage<u64>, String>, u64>, _, _>(close, &mut message_stream)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(message, Some(Ok(200)));
  }
}
