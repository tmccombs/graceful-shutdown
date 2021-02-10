#![cfg_attr(docsrs, feature(doc_cfg))]
//#![deny(missing_docs)]
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

const MAX_PENDING: usize = usize::MAX >> 1;
const ACTIVE_STATE: usize = !MAX_PENDING;

/// Future for graceful shutdown.
pub struct Shutdown {
    inner: Arc<Inner>,
}

pin_project! {
    pub struct WithTerminator<T: Future<Output=()>> {
        inner: Arc<Inner>,
        #[pin]
        terminator: T,
    }
}

/// A shared reference to a Shutdown.
///
/// This allows communicating with a `Shutdown` object without
/// needing ownership to the
#[derive(Clone)]
pub struct Handle(Arc<Inner>);

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
    waker: Mutex<Option<Waker>>,
}

pin_project! {
    pub struct Graceful<F: Future> {
        shutdown: Arc<Inner>,
        #[pin]
        task: F,
    }
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: AtomicUsize::new(ACTIVE_STATE),
                waker: Mutex::new(None),
            }),
        }
    }

    /// Get a reference to manage the `Shutdown`
    pub fn handle(&self) -> Handle {
        Handle(self.inner.clone())
    }

    /// Add an early termination condition.
    ///
    /// After shutdown has been initiated, the `terminator` future will be run, and if it completes
    /// before all tasks are completed the shutdown future will complete, thus finishing the
    /// shutdown even if there are outstanding tasks. This can be useful for using a timeout or
    /// signal (or combination) to force a full shutdown even if one or more tasks are taking
    /// longer than expected to finish.
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
            inner: self.inner,
            terminator,
        }
    }

    /// Convenience function to add a timeout for termination using a
    /// [Tokio Sleep](https://docs.rs/tokio/1.2.0/tokio/time/struct.Sleep.html)
    #[cfg(feature = "tokio-timeout")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio-timeout")))]
    pub fn with_timeout(self, dur: Duration) -> impl Future<Output = ()> {
        self.with_terminator(async move {
            tokio::time::sleep(dur).await;
        })
    }

    /// Convenience function to add a timeout for termination using an
    /// [async-io Timer](https://docs.rs/async-io/1.3.1/async_io/struct.Timer.html)
    #[cfg(feature = "async-io-timeout")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async-io-timeout")))]
    pub fn with_timer(self, dur: Duration) -> impl Future<Output = ()> {
        self.with_terminator(async move {
            async_io::Timer::after(dur).await;
        })
    }
}

impl Handle {
    /// Initiate shutdown.
    ///
    /// This signals that a graceful shutdown shold be started. After calling this
    /// `is_shutting_down` will return true for any reference to the same `Shutdown`
    /// instance. And once all pending tasks are completed, the `Shutdown` future will
    /// complete.
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
    /// let interrupt = shutdown.handle().shutdown_after(ctrl_c());
    /// async {
    ///     interrupt.await.expect("Unable to listen to signal");
    ///     // peform additional cleanup before shutting down
    ///     cleanup();
    ///     shutdown.await;
    /// }
    /// # ;
    /// ```
    pub async fn shutdown_after<F: Future>(self, f: F) -> F::Output {
        let result = f.await;
        self.shutdown();
        result
    }


    #[inline]
    pub fn is_shutting_down(&self) -> bool {
        self.0.is_shutting_down()
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        !self.is_shutting_down()
    }

    /// Wrap a future so that it prevents the `Shutdown` future from completing until
    /// after this future completes.
    pub fn graceful<F: Future>(&self, task: F) -> Graceful<F> {
        self.0.start_task();
        Graceful {
            shutdown: self.0.clone(),
            task,
        }
    }

    /// Wrap a stream so that it stops producing items once shutdown is initiated.
    ///
    /// The resulting stream returns the same results as the original stream, but
    /// if a shutdown has been initiated, and the wrapped stream is pending, then the stream
    /// will return `None` on the next poll, indicating the stream is closed. The inner stream
    /// will also be dropped at that point.
    ///
    /// This is useful for something like a TCP listener socket, so that you can have it stop
    /// accepting new connections once shutdown has been initiated.
    #[cfg(feature = "stream")]
    pub fn graceful_stream<S: Stream>(&self, stream: S) -> GracefulStream<S> {
        GracefulStream::new(self.clone(), stream)
    }

    /// Return an object that prevents the `Shutdown` future from completing while it is live.
    pub fn draining(&self) -> Draining {
        self.0.start_task();
        Draining(self.0.clone())
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
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }

    fn is_shutting_down(&self) -> bool {
        (self.state.load(Ordering::Relaxed) & ACTIVE_STATE) == 0
    }

    fn set_waker(&self, cx: &mut Context<'_>) {
        *self.waker.lock().unwrap() = Some(cx.waker().clone());
    }
}

impl Future for Shutdown {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = &self.inner;
        if inner.state.load(Ordering::Relaxed) == 0 {
            Poll::Ready(())
        } else {
            inner.set_waker(cx);
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
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let state = self.inner.state.load(Ordering::Relaxed);
        if state & ACTIVE_STATE == 0 {
            if state == 0 {
                Poll::Ready(())
            } else {
                self.inner.set_waker(cx);
                self.project().terminator.poll(cx)
            }
        } else {
            self.inner.set_waker(cx);
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
