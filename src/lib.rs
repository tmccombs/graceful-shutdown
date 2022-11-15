#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, feature(extended_key_value_attributes))]
#![deny(missing_docs)]

//! A library to make it easier to handle graceful shutdowns in async code.
//!
//! This provides tools to wait for pending tasks to complete before finalizing shutdown.
//!
//! ## Examples
//!
//! ```no_run
//! use graceful_shutdown::Shutdown;
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//! use tokio::net::TcpListener;
//! use tokio::signal;
//! use tokio::{select, spawn};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let shutdown = Shutdown::new();
//!     let listener = TcpListener::bind("127.0.0.1:8000").await?;
//!     spawn(shutdown.shutdown_after(signal::ctrl_c()));
//!     loop {
//!        match shutdown.cancel_on_shutdown(listener.accept()).await {
//!            Some(Ok((mut conn, _))) => {
//!                spawn(shutdown.graceful(async move {
//!                    let mut buf = [0; 1024];
//!                    loop {
//!                        let n = match conn.read(&mut buf).await {
//!                            Ok(n) if n == 0 => return,
//!                            Ok(n) => n,
//!                            Err(e) => {
//!                                eprintln!("failed to read from socket; err = {:?}", e);
//!                                return;
//!                            }
//!                        };
//!                        if let Err(e) = conn.write_all(&buf[0..n]).await {
//!                            eprintln!("failed to write to socket; err = {:?}", e);
//!                            return;
//!                        }
//!                    }
//!                }));
//!            }
//!            Some(Err(e)) => {
//!                eprintln!("Error accepting connection; err = {:?}", e);
//!                shutdown.shutdown();
//!            }
//!            None => {
//!                eprintln!("Starting shutdown");
//!                break;
//!            }
//!        }
//!     }
//!     shutdown.await;
//!     Ok(())
//! }
//! ```
//!

#[cfg(feature = "stream")]
use futures_core::stream::Stream;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
#[cfg(any(feature = "tokio-timeout", feature = "async-io-timeout"))]
use std::time::Duration;

mod cancel;
pub use cancel::CancelOnShutdown;

const MAX_PENDING: usize = usize::MAX >> 1;
const ACTIVE_STATE: usize = !MAX_PENDING;

/// Future for graceful shutdown.
///
/// This is a [`Future`](std::future::Future) which doesn't complete until a shutdown has been
/// initiated (for example with [`Shutdown::shutdown`]) and any pending tasks have been completed.
///
/// It also contains associated functions for keeping track of tasks that need to be completed
/// before shutdown and an [`initiated`](Shutdown::initiated) function to create a future that only
/// waits until shutdown has been initiated.
#[derive(Clone)]
pub struct Shutdown(Arc<Inner>);

pin_project! {
    /// Future for waiting for a shutdown with an escape hatch terminating condition.
    ///
    /// See [`Shutdown::with_terminator`].
    pub struct WithTerminator<T: Future<Output=()>> {
        inner: Arc<Inner>,
        #[pin]
        terminator: T,
    }
}

/// A drop guard to prevent final shutdown.
///
/// This can be used to track a task that should be completed before shutdown is
/// completed. As long as it is alive, shutdown will be prevented, and dropping it
/// reduces the count of pending tasks before final shutdown.
#[derive(Clone)]
pub struct Draining(Arc<Inner>);

struct Inner {
    /// This is an integer that encodes the current state as follows:
    /// The most significant bit is 0 if a shutdown has been initiated, and 1 if it
    /// has not. The remaining bits are used to keep track of the number of pending
    /// tasks. Note that this sets a limit of `usize::MAX >> 1`
    state: AtomicUsize,
    wakers: Mutex<Vec<Waker>>,
}

pin_project! {
    /// Future that prevents shutting down until the wrapped future completes.
    ///
    /// See [`Shutdown::graceful`].
    pub struct Graceful<F: Future> {
        shutdown: Arc<Inner>,
        #[pin]
        task: F,
    }
}

impl Shutdown {
    /// Createa a new [`Shutdown`].
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            state: AtomicUsize::new(ACTIVE_STATE),
            // We use an initial capacity of 2, because there probably won't be more
            // than 2 futures waiting on this at a time.
            wakers: Mutex::new(Vec::with_capacity(2)),
        }))
    }

    /// Add an early termination condition.
    ///
    /// After shutdown has been initiated, the `terminator` future will be run, and if it completes
    /// before all tasks are completed the shutdown future will complete, thus finishing the
    /// shutdown even if there are outstanding tasks. This can be useful for using a timeout or
    /// signal (or combination) to force a full shutdown even if one or more tasks are taking
    /// longer than expected to finish.
    ///
    /// The returned future has a boolean output that is `true` if the future was terminated early
    /// due to the termination condition, and `false` if all remaining tasks completed normally.
    ///
    /// Note that `terminator` will not start to be polled until after shutdown has been initiated.
    /// However, you may need to delay creation of the actual timeout future until
    /// the first poll, so that the end time is set correctly. This can be done using something
    /// like
    /// ```
    /// # #[cfg(feature = "tokio-timeout")] {
    /// # use std::time::Duration;
    /// #
    /// let shutdown = Shutdown::new().with_terminator(async {
    ///     tokio::time::sleep(Duration::from_secs(30)).await;
    /// });
    /// # }
    /// ```
    pub fn with_terminator<T: Future<Output = ()>>(self, terminator: T) -> WithTerminator<T> {
        WithTerminator {
            inner: self.0,
            terminator,
        }
    }

    /// Convenience function to add a timeout for termination using a
    /// [Tokio Sleep](https://docs.rs/tokio/1.2.0/tokio/time/struct.Sleep.html).
    ///
    /// The future returns `true` if the timeout triggered, and `false` otherwise.
    ///
    /// See [`Shutdown::with_terminator`].
    #[cfg(feature = "tokio-timeout")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio-timeout")))]
    pub fn with_timeout(self, dur: Duration) -> impl Future<Output = bool> {
        self.with_terminator(async move {
            tokio::time::sleep(dur).await;
        })
    }

    /// Convenience function to add a timeout for termination using an
    /// [async-io Timer](https://docs.rs/async-io/1.3.1/async_io/struct.Timer.html)
    ///
    /// The future returns `true` if the timeout triggered, and `false` otherwise.
    ///
    /// See [`Shutdown::with_terminator`].
    #[cfg(feature = "async-io-timeout")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async-io-timeout")))]
    pub fn with_timer(self, dur: Duration) -> impl Future<Output = bool> {
        self.with_terminator(async move {
            async_io::Timer::after(dur).await;
        })
    }

    /// Initiate shutdown.
    ///
    /// This signals that a graceful shutdown shold be started. After calling this
    /// [`is_shutting_down`](Shutdown::is_shutting_down) will return true for any reference to the
    /// same `Shutdown` instance. And once all pending tasks are completed, the `Shutdown` future
    /// will complete.
    pub fn shutdown(&self) {
        self.0.shutdown();
    }

    /// Return a new future that waits for `f` to complete, then initiates shutdown.
    ///
    /// # Examples
    ///
    /// ```
    /// # use graceful_shutdown::Shutdown;
    /// # async fn ctrl_c() -> Result<(), ()> { Ok(()) }
    /// # fn cleanup() { }
    /// let shutdown = Shutdown::new();
    /// let interrupt = shutdown.shutdown_after(ctrl_c());
    /// async {
    ///     interrupt.await.expect("Unable to listen to signal");
    ///     // peform additional cleanup before shutting down
    ///     cleanup();
    ///     shutdown.await;
    /// }
    /// # ;
    /// ```
    pub fn shutdown_after<F: Future>(&self, f: F) -> impl Future<Output = F::Output> {
        let handle = self.clone();
        async move {
            let result = f.await;
            handle.shutdown();
            result
        }
    }

    /// Return true if shutdown as been initiated.
    pub fn is_shutting_down(&self) -> bool {
        self.0.is_shutting_down()
    }

    /// Return true if shutdown has not yet been initiated.
    pub fn is_active(&self) -> bool {
        !self.is_shutting_down()
    }

    /// Return how many tasks are currently pending.
    ///
    /// This should not be relied on to be completely accurate in a multi-threaded context.
    /// However, it may be useful for reporting approximately how much work still needs to be done
    /// before shutting down the program.
    pub fn num_pending(&self) -> usize {
        self.0.state.load(Ordering::Relaxed) & MAX_PENDING
    }

    /// Wrap a future so that it prevents the [`Shutdown`] future from completing until
    /// after this future completes.
    pub fn graceful<F: Future>(&self, task: F) -> Graceful<F> {
        self.0.start_task();
        Graceful {
            shutdown: self.0.clone(),
            task,
        }
    }

    /// Wrap a future so that it is cancelled if shutdown is initiated.
    ///
    /// If the future completes the result is returned in a `Some`. If it is canceled
    /// due to shutdown, `None` is returned.
    ///
    /// # Examples
    ///
    /// Event loop with shutdown:
    /// ```
    /// # use std::future;
    /// # use graceful_shutdown::Shutdown;
    /// # fn getEvent() -> impl future::Future<Output=()> {
    /// #   future::pending()
    /// # }
    /// # #[tokio::main]
    /// # async fn main() {
    /// let shutdown = Shutdown::new();
    ///
    /// # shutdown.shutdown();
    ///
    /// loop {
    ///     match shutdown.cancel_on_shutdown(getEvent()).await {
    ///         Some(_) => unimplemented!(),
    ///         None => break,
    ///     }
    /// }
    ///
    /// shutdown.await;
    ///
    /// # }
    /// ```
    ///
    /// Future that waits for shutdown to be initiated:
    /// ```
    /// use graceful_shutdown::Shutdown;
    /// let shutdown = Shutdown::new();
    /// let future = shutdown.cancel_on_shutdown(std::future::pending::<()>());
    /// ```
    pub fn cancel_on_shutdown<F: Future>(&self, future: F) -> CancelOnShutdown<F> {
        CancelOnShutdown::new(self.clone(), future)
    }

    /// Wrap a [`Stream`](https://docs.rs/futures-core/0.3/futures_core/stream/trait.Stream.html)
    /// so that it stops producing items once shutdown is initiated.
    ///
    /// The resulting stream returns the same results as the original stream, but if a shutdown has
    /// been initiated, and the wrapped stream is pending, then the stream will return
    /// [`None`](std::option::Option::None) on the next poll, indicating the stream is closed. The
    /// inner stream will also be dropped at that point.
    ///
    /// This is useful for something like a TCP listener socket, so that you can have it stop
    /// accepting new connections once shutdown has been initiated.
    #[cfg(feature = "stream")]
    pub fn graceful_stream<S: Stream>(&self, stream: S) -> GracefulStream<S> {
        GracefulStream::new(self.clone(), stream)
    }

    /// Return an object that prevents the [`Shutdown`] future from completing while it is live.
    pub fn draining(&self) -> Draining {
        self.0.start_task();
        Draining(self.0.clone())
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown::new()
    }
}

impl Inner {
    fn shutdown(&self) {
        // Clear the "active" flag.
        self.state.fetch_and(MAX_PENDING, Ordering::Relaxed);
        self.wake();
    }

    fn start_task(&self) {
        assert_ne!(
            self.state.fetch_add(1, Ordering::Relaxed),
            MAX_PENDING,
            "Exceeding maximum number of pending tasks"
        );
    }

    fn end_task(&self) {
        if self.state.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.wake();
        }
    }

    fn wake(&self) {
        for waker in self.wakers.lock().unwrap().drain(..) {
            waker.wake();
        }
    }

    fn is_shutting_down(&self) -> bool {
        (self.state.load(Ordering::Relaxed) & ACTIVE_STATE) == 0
    }

    fn add_waker(&self, cx: &mut Context<'_>) {
        let mut wakers = self.wakers.lock().unwrap();
        if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
            wakers.push(cx.waker().clone());
        }
    }
}

impl Future for Shutdown {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = &self.0;
        if inner.state.load(Ordering::Relaxed) == 0 {
            Poll::Ready(())
        } else {
            inner.add_waker(cx);
            Poll::Pending
        }
    }
}

impl<F: Future> Future for Graceful<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.task.poll(cx) {
            res @ Poll::Ready(_) => {
                this.shutdown.end_task();
                res
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: Future<Output = ()>> Future for WithTerminator<T> {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let state = self.inner.state.load(Ordering::Relaxed);
        if state & ACTIVE_STATE == 0 {
            if state == 0 {
                Poll::Ready(false)
            } else {
                self.inner.add_waker(cx);
                self.project().terminator.poll(cx).map(|_| true)
            }
        } else {
            self.inner.add_waker(cx);
            Poll::Pending
        }
    }
}

impl Drop for Draining {
    fn drop(&mut self) {
        self.0.end_task();
    }
}

#[cfg(feature = "stream")]
mod stream;
#[cfg(feature = "stream")]
pub use stream::GracefulStream;
