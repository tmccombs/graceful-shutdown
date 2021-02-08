use futures_core::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use super::Handle;
use pin_project_lite::pin_project;

pin_project! {
    #[project = GracefulStreamProj]
    pub enum GracefulStream<S: Stream> {
        Running {
            shutdown: Handle,
            #[pin]
            stream: S,
        },
        Done,
    }
}

impl<S: Stream> GracefulStream<S> {
    pub(crate) fn new(shutdown: Handle, stream: S) -> Self {
        GracefulStream::Running {
            shutdown,
            stream: stream,
        }
    }
}

impl<S: Stream> Stream for GracefulStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use GracefulStreamProj::*;
        match self.as_mut().project() {
            Done => Poll::Ready(None),
            Running { shutdown, stream } => {
                match stream.poll_next(cx) {
                    Poll::Pending if shutdown.is_shutting_down() => {
                        self.set(GracefulStream::Done);
                        Poll::Ready(None)
                    }
                    res => res
                }
            }
        }
    }
}
