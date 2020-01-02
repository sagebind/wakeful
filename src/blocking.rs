use crate::wakers::Wake;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    thread,
};

/// Extension trait that provides methods for blocking synchronously on a
/// future.
pub trait Blocking: Future {
    /// Block the current thread until this future is ready.
    ///
    /// It is not advised to use this inside an async context.
    fn blocking_wait(mut self) -> Self::Output
    where
        Self: Sized,
    {
        let waker = thread::current().into_waker();
        let mut context = Context::from_waker(&waker);
        let mut future = unsafe { Pin::new_unchecked(&mut self) };

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => thread::park(),
            }
        }
    }
}

impl<F: Future> Blocking for F {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_wait() {
        async fn number_async() -> usize {
            42
        }

        assert_eq!(number_async().blocking_wait(), 42);
    }
}
