//! Helpers shared by the integration tests for driving async calls without an
//! async runtime.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

/// Run `first` and `second` together until both are complete.
pub async fn join<A, B>(first: impl Future<Output = A>, second: impl Future<Output = B>) -> (A, B) {
    let mut first = std::pin::pin!(first);
    let mut second = std::pin::pin!(second);
    let (mut a, mut b) = (None, None);
    std::future::poll_fn(|cx| {
        if a.is_none()
            && let Poll::Ready(output) = first.as_mut().poll(cx)
        {
            a = Some(output);
        }
        if b.is_none()
            && let Poll::Ready(output) = second.as_mut().poll(cx)
        {
            b = Some(output);
        }
        if a.is_some() && b.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (
        a.expect("first is complete"),
        b.expect("second is complete"),
    )
}

/// Run `future` to completion on this thread, parking it while the future is
/// not ready.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    struct Unpark(std::thread::Thread);

    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Arc::new(Unpark(std::thread::current())).into();
    let mut cx = TaskContext::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}
