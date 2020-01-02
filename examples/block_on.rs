use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    thread,
};
use wakeful::Wake;

/// Block the current thread until this future is ready.
pub fn block_on<F: Future>(mut future: F) -> F::Output {
    /// Note that this crate already implements `Wake` for `Thread`, this just
    /// demonstrates how simple the implementation is.
    #[derive(Clone)]
    struct ThreadWaker(thread::Thread);

    impl Wake for ThreadWaker {
        fn wake_by_ref(&self) {
            self.0.unpark();
        }
    }

    // Now that we can easily create a waker that does what we want (unpark this
    // thread), it is now easy to create a context and begin polling the given
    // future efficiently.
    let waker = ThreadWaker(thread::current()).into_waker();
    let mut context = Context::from_waker(&waker);
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

async fn number_async() -> usize {
    42
}

fn main() {
    block_on(async {
        assert_eq!(number_async().await, 42);
    });
}
