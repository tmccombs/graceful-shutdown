use super::Shutdown;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    /// A future that is canceled if shutdown is initiated.
    ///
    /// Created by [`Shutdown::cancel_on_shutdown`]. See its documetnation for more.
    pub struct CancelOnShutdown<F> {
        shutdown: Shutdown,
        #[pin]
        inner: F,
    }
}

impl<F> CancelOnShutdown<F> {
    pub(crate) fn new(shutdown: Shutdown, inner: F) -> Self {
        CancelOnShutdown { shutdown, inner }
    }
}

impl<F: Future> Future for CancelOnShutdown<F> {
    type Output = Option<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.as_mut().project().inner.poll(cx) {
            Poll::Ready(v) => Poll::Ready(Some(v)),
            Poll::Pending => {
                if self.shutdown.is_active() {
                    self.shutdown.0.add_waker(cx);
                    Poll::Pending
                } else {
                    Poll::Ready(None)
                }
            }
        }
    }
}
