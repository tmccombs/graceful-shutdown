use super::Shutdown;
use futures_core::stream::Stream;
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    /// A Stream that stops producing items after shutdown has been initiated.
    ///
    /// Created by [`Shutdown::graceful_stream`]. See its documentation for more.
    #[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
    pub struct GracefulStream<S>{
        #[pin]
        state: State<S>,
    }
}

pin_project! {
    #[project = StateProj]
    enum State<S> {
        Running {
            shutdown: Shutdown,
            #[pin]
            stream: S,
        },
        Done,
    }
}

impl<S> GracefulStream<S> {
    pub(crate) fn new(shutdown: Shutdown, stream: S) -> Self {
        GracefulStream {
            state: State::Running {
                shutdown,
                stream: stream,
            },
        }
    }
}

impl<S: Stream> Stream for GracefulStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use StateProj::*;
        let mut state = self.as_mut().project().state;
        match state.as_mut().project() {
            Done => Poll::Ready(None),
            Running { shutdown, stream } => match stream.poll_next(cx) {
                Poll::Pending if shutdown.is_shutting_down() => {
                    state.set(State::Done);
                    Poll::Ready(None)
                }
                res => {
                    shutdown.0.add_waker(cx);
                    res
                }
            },
        }
    }
}
