//#![deny(missing_docs)]
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker, Poll};
#[cfg(feature = "stream")]
use futures_core::stream::Stream;
use pin_project_lite::pin_project;

const MAX_PENDING: usize = usize::MAX >> 1;
const ACTIVE_STATE: usize = !MAX_PENDING;

/// Future for graceful shutdown.
pub struct Shutdown {
    inner: Arc<Inner>,
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
}

impl Handle {
    /// Initiate shutdown.
    ///
    /// This signals that a graceful shutdown shold be started. After calling this
    /// `is_shutting_down` will return true for any reference to the same `Shutdown`
    /// instance. And once all pending tasks are completed, the `Shutdown` future will
    /// complete.
    pub fn shutdown(&self) {
        // Clear the "active" flag.
        self.0.state.fetch_and(MAX_PENDING, Ordering::Relaxed);
        self.0.wake();
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
}

impl Future for Shutdown {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = &self.inner;
        if inner.state.load(Ordering::Relaxed) == 0 {
            Poll::Ready(())
        } else {
            *inner.waker.lock().unwrap() = Some(cx.waker().clone());
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

impl Drop for Draining {
    fn drop(&mut self) {
        self.0.end_task();
    }
}

#[cfg(feature = "stream")]
mod stream;
#[cfg(feature = "stream")]
pub use stream::GracefulStream;

// TODO:
// - Automatically shutdown after a future completes
// - Timeout for Shutdown future (tokio/time feature)
// - Macro to simplify graceful shutdown with timeout.
