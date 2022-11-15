use graceful_shutdown::Shutdown;
use std::task::{Context, Poll};
use std::pin::Pin;
use futures_task::noop_waker_ref;
use std::future::Future;
use tokio::sync;

#[test]
fn basic_flow() {
    let mut cx = Context::from_waker(noop_waker_ref());
    let mut shutdown = Shutdown::new();

    assert!(shutdown.is_active());
    assert!(!shutdown.is_shutting_down());

    {
        let _drain = shutdown.draining();
        shutdown.shutdown();
        assert!(shutdown.is_shutting_down());
        assert!(!shutdown.is_active());
        assert_eq!(Poll::Pending, Pin::new(&mut shutdown).poll(&mut cx));
    }
    assert_eq!(Pin::new(&mut shutdown).poll(&mut cx), Poll::Ready(()));
}

#[tokio::test]
async fn terminator() {
    let (tx, rx) = sync::oneshot::channel();

    let shutdown = Shutdown::new();
    let _drain = shutdown.draining();

    let terminated = shutdown.clone().with_terminator(async move {
        rx.await.unwrap()
    });

    assert!(shutdown.is_active());
    shutdown.shutdown();
    assert!(shutdown.is_shutting_down());
    assert_eq!(shutdown.num_pending(),  1);
    tx.send(()).unwrap();
    assert!(terminated.await);
}
