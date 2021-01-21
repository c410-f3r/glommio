// Unless explicitly stated otherwise all files in this repository are licensed under the
// MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//
//
use crate::parking::Reactor;
use crate::{
    channels::spsc_queue::{make, BufferHalf, Consumer, Producer},
    GlommioError, ResourceType,
};
use crate::{enclose, Local};
use futures_lite::future;
use futures_lite::stream::Stream;
use std::fmt;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};

type Result<T, V> = crate::Result<T, V>;

/// The `SharedReceiver` is the receiving end of the Shared Channel.
/// It implements [`Send`] so it can be passed to any thread. However
/// it doesn't implement any method: before it is used it must be changed
/// into a [`ConnectedReceiver`], which then makes sure it will be used by
/// at most one thread.
///
/// It is technically possible to share this among multiple threads inside
/// a lock, although such design is discouraged and beats the purpose of a
/// spsc channel.
///
/// [`ConnectedReceiver`]: struct.ConnectedReceiver.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
pub struct SharedReceiver<T: Send + Sized + Copy> {
    state: Option<Rc<ReceiverState<T>>>,
}

/// The `SharedSender` is the sending end of the Shared Channel.
/// It implements [`Send`] so it can be passed to any thread. However
/// it doesn't implement any method: before it is used it must be changed
/// into a [`ConnectedSender`], which then makes sure it will be used by
/// at most one thread.
///
/// It is technically possible to share this among multiple threads inside
/// a lock, although such design is discouraged and beats the purpose of a
/// spsc channel.
///
/// [`ConnectedSender`]: struct.ConnectedSender.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
pub struct SharedSender<T: Send + Sized + Copy> {
    state: Option<Rc<SenderState<T>>>,
}

impl<T: Send + Sized + Copy> fmt::Debug for SharedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            Some(s) => write!(f, "Unbound SharedSender {:?}", s.buffer),
            None => write!(f, "Bound SharedSender"),
        }
    }
}

impl<T: Send + Sized + Copy> fmt::Debug for SharedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            Some(s) => write!(f, "Unbound SharedReceiver: {:?}", s.buffer),
            None => write!(f, "Bound SharedReceiver"),
        }
    }
}

unsafe impl<T: Send + Sized + Copy> Send for SharedReceiver<T> {}
unsafe impl<T: Send + Sized + Copy> Send for SharedSender<T> {}

/// The `ConnectedReceiver` is the receiving end of the Shared Channel.
pub struct ConnectedReceiver<T: Send + Sized + Copy> {
    id: u64,
    state: Rc<ReceiverState<T>>,
    reactor: Weak<Reactor>,
}

/// The `ConnectedReceiver` is the sending end of the Shared Channel.
pub struct ConnectedSender<T: Send + Sized + Copy> {
    id: u64,
    state: Rc<SenderState<T>>,
    reactor: Weak<Reactor>,
}

impl<T: Send + Sized + Copy> fmt::Debug for ConnectedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Connected Receiver {}: {:?}", self.id, self.state.buffer)
    }
}

impl<T: Send + Sized + Copy> fmt::Debug for ConnectedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Connected Sender {} : {:?}", self.id, self.state.buffer)
    }
}

struct SenderState<T: Send + Sized + Copy> {
    buffer: Producer<T>,
}

struct ReceiverState<T: Send + Sized + Copy> {
    buffer: Consumer<T>,
}

/// Creates a a new `shared_channel` returning its sender and receiver endpoints.
///
/// All shared channels must be bounded.
pub fn new_bounded<T: Send + Sized + Copy>(size: usize) -> (SharedSender<T>, SharedReceiver<T>) {
    let (producer, consumer) = make(size);
    (
        SharedSender {
            state: Some(Rc::new(SenderState { buffer: producer })),
        },
        SharedReceiver {
            state: Some(Rc::new(ReceiverState { buffer: consumer })),
        },
    )
}

impl<T: 'static + Send + Sized + Copy> SharedSender<T> {
    /// Connects this sender, returning a [`ConnectedSender`] that can be used
    /// to send data into this channel
    ///
    /// [`ConnectedSender`]: struct.ConnectedSender.html
    pub fn connect(mut self) -> ConnectedSender<T> {
        let state = self.state.take().unwrap();
        let reactor = Local::get_reactor();
        state.buffer.connect(reactor.eventfd());
        let id = reactor.register_shared_channel(Box::new(enclose! {(state) move || {
            if state.buffer.consumer_disconnected() {
                state.buffer.capacity()
            } else {
                state.buffer.free_space()
            }
        }}));

        let reactor = Rc::downgrade(&reactor);
        ConnectedSender { state, id, reactor }
    }
}

impl<T: Send + Sized + Copy> ConnectedSender<T> {
    /// Sends data into this channel.
    ///
    /// It returns a [`GlommioError::Closed`] if the receiver is destroyed.
    /// It returns a [`GlommioError::WouldBlock`] if this is a bounded channel that has no more capacity
    ///
    /// # Examples
    /// ```
    /// use glommio::prelude::*;
    /// use glommio::channels::shared_channel;
    /// use futures_lite::StreamExt;
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let (sender, receiver) = shared_channel::new_bounded(1);
    ///     let sender = sender.connect();
    ///     let mut receiver = receiver.connect();
    ///     sender.try_send(0);
    ///     sender.try_send(0).unwrap_err(); // no more capacity
    ///     receiver.next().await.unwrap(); // now we have capacity again
    ///     drop(receiver); // but because the receiver is destroyed send will err
    ///     sender.try_send(0).unwrap_err();
    /// });
    /// ```
    ///
    /// [`BrokenPipe`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.BrokenPipe
    /// [`WouldBlock`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.WouldBlock
    /// [`Other`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.Other
    /// [`GlommioError`]: ../../struct.GlommioError.html
    pub fn try_send(&self, item: T) -> Result<(), T> {
        // This is a shared channel so state can change under our noses.
        // We test if the buffer is disconnected before sending to avoid
        // sending a value that will not be received (otherwise we would only
        // receive WouldBlock when the buffer capacity fills).
        //
        // However after we try_push(), we can still fail because the buffer
        // disconnected between now and then. That's okay as all we're trying to
        // do here is prevent unnecessary sends.
        if self.state.buffer.consumer_disconnected() {
            return Err(GlommioError::Closed(ResourceType::Channel(item)));
        }
        match self.state.buffer.try_push(item) {
            None => {
                if let Some(fd) = self.state.buffer.must_notify() {
                    self.reactor.upgrade().unwrap().notify(fd);
                }
                Ok(())
            }
            Some(item) => {
                let res = if self.state.buffer.consumer_disconnected() {
                    GlommioError::Closed(ResourceType::Channel(item))
                } else {
                    GlommioError::WouldBlock(ResourceType::Channel(item))
                };
                Err(res)
            }
        }
    }

    /// Sends data into this channel when it is ready to receive it
    ///
    /// # Examples
    /// ```
    /// use glommio::prelude::*;
    /// use glommio::channels::shared_channel;
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let (sender, receiver) = shared_channel::new_bounded(1);
    ///     let sender = sender.connect();
    ///     let receiver = receiver.connect();
    ///     sender.send(0).await.unwrap();
    /// });
    /// ```
    pub async fn send(&self, item: T) -> Result<(), T> {
        let waiter = future::poll_fn(|cx| self.wait_for_room(cx));
        waiter.await;
        let res = self.try_send(item);
        if let Err(GlommioError::WouldBlock(_)) = &res {
            panic!("operation would block")
        }
        res
    }

    fn wait_for_room(&self, cx: &mut Context<'_>) -> Poll<()> {
        match self.state.buffer.free_space() > 0 {
            true => Poll::Ready(()),
            false => {
                self.reactor
                    .upgrade()
                    .unwrap()
                    .add_shared_channel_waker(self.id, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T: 'static + Send + Sized + Copy> SharedReceiver<T> {
    /// Connects this receiver, returning a [`ConnectedReceiver`] that can be used
    /// to send data into this channel
    ///
    /// [`ConnectedReceiver`]: struct.ConnectedReceiver.html
    pub fn connect(mut self) -> ConnectedReceiver<T> {
        let reactor = Local::get_reactor();
        let state = self.state.take().unwrap();
        state.buffer.connect(reactor.eventfd());
        let id = reactor.register_shared_channel(Box::new(enclose! { (state) move || {
            if state.buffer.producer_disconnected() {
                state.buffer.capacity()
            } else {
                state.buffer.size()
            }
        }}));

        let reactor = Rc::downgrade(&reactor);
        ConnectedReceiver { state, id, reactor }
    }
}

impl<T: Send + Sized + Copy> ConnectedReceiver<T> {
    /// Receives data from this channel
    ///
    /// If the sender is no longer available it returns [`None`]. Otherwise block until
    /// an item is available and returns it wrapped in [`Some`]
    ///
    /// Notice that this is also available as a Stream. Whether to consume from a stream
    /// or `recv` is up to the application. The biggest difference is that [`StreamExt`]'s
    /// [`next`] method takes a mutable reference to self. If the LocalReceiver is, say,
    /// behind an [`Rc`] it may be more ergonomic to recv.
    ///
    /// # Examples
    /// ```
    /// use glommio::prelude::*;
    /// use glommio::channels::shared_channel;
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let (sender, receiver) = shared_channel::new_bounded(1);
    ///     let sender = sender.connect();
    ///     let receiver = receiver.connect();
    ///     sender.send(0).await.unwrap();
    ///     let x = receiver.recv().await.unwrap();
    ///     assert_eq!(x, 0);
    /// });
    /// ```
    ///
    /// [`None`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.None
    /// [`Some`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.Some
    /// [`StreamExt`]: https://docs.rs/futures-lite/1.11.2/futures_lite/stream/index.html
    /// [`next`]: https://docs.rs/futures-lite/1.11.2/futures_lite/stream/trait.StreamExt.html#method.next
    /// [`Rc`]: https://doc.rust-lang.org/std/rc/struct.Rc.html
    pub async fn recv(&self) -> Option<T> {
        let waiter = future::poll_fn(|cx| self.recv_one(cx));
        waiter.await
    }

    fn recv_one(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        match self.state.buffer.try_pop() {
            None if !self.state.buffer.producer_disconnected() => {
                self.reactor
                    .upgrade()
                    .unwrap()
                    .add_shared_channel_waker(self.id, cx.waker().clone());
                Poll::Pending
            }
            res => {
                if let Some(fd) = self.state.buffer.must_notify() {
                    self.reactor.upgrade().unwrap().notify(fd);
                }
                Poll::Ready(res)
            }
        }
    }
}

impl<T: Send + Sized + Copy> Stream for ConnectedReceiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.recv_one(cx)
    }
}

impl<T: Send + Sized + Copy> Drop for SharedSender<T> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            // Never connected, we must connect ourselves.
            state.buffer.disconnect();
            if let Some(fd) = state.buffer.must_notify() {
                Local::get_reactor().notify(fd);
            }
        }
    }
}

impl<T: Send + Sized + Copy> Drop for SharedReceiver<T> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            // Never connected, we must connect ourselves.
            state.buffer.disconnect();
            if let Some(fd) = state.buffer.must_notify() {
                Local::get_reactor().notify(fd);
            }
        }
    }
}

impl<T: Send + Sized + Copy> Drop for ConnectedReceiver<T> {
    fn drop(&mut self) {
        self.state.buffer.disconnect();
        if let Some(fd) = self.state.buffer.must_notify() {
            if let Some(r) = self.reactor.upgrade() {
                r.notify(fd);
            }
        }
        if let Some(r) = self.reactor.upgrade() {
            r.unregister_shared_channel(self.id)
        }
    }
}

impl<T: Send + Sized + Copy> Drop for ConnectedSender<T> {
    fn drop(&mut self) {
        self.state.buffer.disconnect();
        if let Some(fd) = self.state.buffer.must_notify() {
            if let Some(r) = self.reactor.upgrade() {
                r.notify(fd);
            }
        }
        if let Some(r) = self.reactor.upgrade() {
            r.unregister_shared_channel(self.id)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::timer::Timer;
    use crate::LocalExecutorBuilder;
    use futures_lite::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn producer_consumer() {
        let (sender, receiver) = new_bounded(10);

        let ex1 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let sender = sender.connect();
                Timer::new(Duration::from_millis(10)).await;
                sender.try_send(100).unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let receiver = receiver.connect();
                let x = receiver.recv().await;
                assert_eq!(x.unwrap(), 100);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn producer_stream_consumer() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::new()
            .pin_to_cpu(0)
            .spin_before_park(Duration::from_millis(1000000))
            .spawn(move || async move {
                let sender = sender.connect();
                for _ in 0..10 {
                    sender.send(1).await.unwrap();
                    Timer::new(Duration::from_millis(1)).await;
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .pin_to_cpu(1)
            .spin_before_park(Duration::from_millis(1000000))
            .spawn(move || async move {
                let receiver = receiver.connect();
                let sum = receiver.fold(0, |acc, x| acc + x).await;
                assert_eq!(sum, 10);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn consumer_sleeps_before_producer_produces() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                Timer::new(Duration::from_millis(100)).await;
                let sender = sender.connect();
                sender.send(1).await.unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let receiver = receiver.connect();
                let recv = receiver.recv().await.unwrap();
                assert_eq!(recv, 1);
                let sum = receiver.fold(0, |acc, x| acc + x).await;
                assert_eq!(sum, 0);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn producer_sleeps_before_consumer_consumes() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let sender = sender.connect();
                // This will go right away because the channel fits 1 element
                sender.try_send(1).unwrap();
                // This will sleep. The consumer should unblock us
                sender.send(1).await.unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                Timer::new(Duration::from_millis(100)).await;
                let receiver = receiver.connect();
                let sum = receiver.fold(0, |acc, x| acc + x).await;
                assert_eq!(sum, 2);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn producer_never_connects() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                drop(sender);
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let receiver: ConnectedReceiver<usize> = receiver.connect();
                assert_eq!(receiver.recv().await.is_none(), true);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn consumer_never_connects() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                drop(receiver);
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                Timer::new(Duration::from_millis(100)).await;
                let sender: ConnectedSender<usize> = sender.connect();
                match sender.send(0).await {
                    Ok(_) => panic!("Should not have sent"),
                    Err(GlommioError::Closed(ResourceType::Channel(_))) => {
                        // all good
                    }
                    Err(other_err) => {
                        panic!(
                            "incorrect error type: '{}' for channel send",
                            other_err.to_string()
                        )
                    }
                }
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn pass_function() {
        let (sender, receiver) = new_bounded(10);

        let ex1 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let sender = sender.connect();
                Timer::new(Duration::from_millis(10)).await;
                if sender.send(|| 32).await.is_err() {
                    panic!("send failed");
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let receiver = receiver.connect();
                let x = receiver.recv().await.unwrap();
                assert_eq!(32, x());
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn send_to_full_channel() {
        let (sender, receiver) = new_bounded(1);

        let status = Arc::new(AtomicUsize::new(0));
        let s1 = status.clone();

        let ex1 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let sender = sender.connect();
                sender.send(0).await.unwrap();
                let x = sender.try_send(1);
                assert_eq!(x.is_err(), true);
                s1.store(1, Ordering::Relaxed);
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new()
            .spawn(move || async move {
                let receiver = receiver.connect();

                while status.load(Ordering::Relaxed) == 0 {}
                let x = receiver.recv().await.unwrap();
                assert_eq!(0, x);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }
}
