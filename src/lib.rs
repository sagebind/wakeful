//! Utilities to aid implementing [`Waker`s](std::task::Waker) and working with
//! tasks.
//!
//! The highlight of this crate is [`Wake`], which allows you to construct
//! wakers from your own types by implementing this trait.
//!
//! # Examples
//!
//! Implementing your own `block_on` function using this crate:
//!
//! ```
//! use std::{
//!     future::Future,
//!     pin::Pin,
//!     task::{Context, Poll},
//!     thread,
//! };
//! use wakeful::Wake;
//!
//! fn block_on<F: Future>(mut future: F) -> F::Output {
//!     let waker = thread::current().into_waker();
//!     let mut context = Context::from_waker(&waker);
//!     let mut future = unsafe { Pin::new_unchecked(&mut future) };
//!
//!     loop {
//!         match future.as_mut().poll(&mut context) {
//!             Poll::Ready(output) => return output,
//!             Poll::Pending => thread::park(),
//!         }
//!     }
//! }
//! ```

#![warn(
    future_incompatible,
    missing_debug_implementations,
    missing_docs,
    rust_2018_idioms,
    unreachable_pub,
    unused,
    clippy::all
)]

use std::{
    mem, ptr,
    sync::Arc,
    task::{RawWaker, RawWakerVTable, Waker},
};

/// Zero-cost helper trait that makes it easier to implement wakers.
///
/// Implementing this trait provides you with [`Wake::into_waker`], which allows
/// you to construct a [`Waker`] from any type implementing [`Wake`]. The only
/// method you must implement is [`Wake::wake_by_ref`] which can encapsulate all
/// your custom wake-up behavior.
///
/// Your custom wakers must also implement [`Clone`], [`Send`], and [`Sync`] to
/// comply with the contract of [`Waker`]. You are free to choose any strategy
/// you like to handle cloning; bundling your state in an inner [`Arc`] is
/// common and plays nicely with this trait.
///
/// # Provided implementations
///
/// A simple waker implementation is provided for [`std::thread::Thread`], which
/// merely calls `unpark()`. This almost trivializes implementing your own
/// single-threaded `block_on` executor. An example of this is provided in the
/// `examples/` directory.
///
/// # Optimizations
///
/// If the size of `Self` is less than or equal to pointer size, as an
/// optimization the underlying implementation will pass `self` in directly to
/// [`RawWakerVTable`] functions. For types larger than a pointer, an allocation
/// will be made on creation and when cloning.
///
/// # Examples
///
/// ```
/// use wakeful::Wake;
///
/// /// Doesn't actually do anything except print a message when wake is called.
/// #[derive(Clone)]
/// struct PrintWaker;
///
/// impl Wake for PrintWaker {
///     fn wake_by_ref(&self) {
///         println!("wake called!");
///     }
/// }
///
/// let waker = PrintWaker.into_waker();
/// waker.wake(); // prints "wake called!"
/// ```
///
/// ```
/// use std::task::Waker;
/// use wakeful::Wake;
///
/// /// Delegates wake calls to multiple wakers.
/// #[derive(Clone)]
/// struct MultiWaker(Vec<Waker>);
///
/// impl Wake for MultiWaker {
///     fn wake(self) {
///         for waker in self.0 {
///             waker.wake();
///         }
///     }
///
///     fn wake_by_ref(&self) {
///         for waker in &self.0 {
///             waker.wake_by_ref();
///         }
///     }
/// }
/// ```
pub trait Wake: Send + Sync + Clone {
    /// Wake up the task associated with this waker, consuming the waker. When
    /// converted into a waker handle, this method is invoked whenever
    /// [`Waker::wake`] is called.
    ///
    /// By default, this delegates to [`Wake::wake_by_ref`], but can be
    /// overridden if a more efficient owned implementation is possible.
    fn wake(self) {
        self.wake_by_ref();
    }

    /// Wake up the task associated with this waker, consuming the waker. When
    /// converted into a waker handle, this method is invoked whenever
    /// [`Waker::wake_by_ref`] is called.
    fn wake_by_ref(&self);

    /// Convert this into a [`Waker`] handle.
    fn into_waker(self) -> Waker {
        // There's a fair bit of magic going on here, so watch out. There are
        // two possible implementations for this function, and which one we
        // invoke is decided at compile time based on the memory size of `Self`.
        //
        // When the size of `Self` is less than or equal to pointer size, we can
        // avoid allocations altogether by treating the data pointer used in the
        // waker vtable as the waker itself.
        //
        // If `Self` is larger than a pointer, then we take the more obvious
        // approach of putting the waker on the heap and passing around a
        // pointer to it.
        //
        // The pointer-size optimization is extremely useful when you want to
        // combine your waker implementation with things like `Arc`, which is
        // already pointer sized. With this approach, such wakers automatically
        // use the best possible implementation as the arc pointer is
        // essentially being passed around directly with no indirection without
        // any extra effort from the implementer.

        /// Convert a wake into a [`RawWaker`] handle.
        fn create_raw_waker<W: Wake>(wake: W) -> RawWaker {
            if mem::size_of::<W>() <= mem::size_of::<*const ()>() {
                create_thin(wake)
            } else {
                create_boxed(wake)
            }
        }

        /// Convert a wake into a [`RawWaker`] handle by allocating a box.
        ///
        /// This is the easier implementation to understand. We create a data
        /// pointer by moving self into a box and then getting its raw pointer.
        fn create_boxed<W: Wake>(wake: W) -> RawWaker {
            RawWaker::new(
                Box::into_raw(Box::new(wake)) as *const (),
                &RawWakerVTable::new(
                    |data| unsafe {
                        create_raw_waker((&*(data as *const W)).clone())
                    },
                    |data| unsafe {
                        Box::from_raw(data as *mut W).wake();
                    },
                    |data| unsafe {
                        (&*(data as *const W)).wake_by_ref();
                    },
                    |data| unsafe {
                        Box::from_raw(data as *mut W);
                    },
                ),
            )
        }

        /// Convert a wake into a [`RawWaker`] handle by transmuting into a data
        /// pointer.
        ///
        /// This is the trickier implementation, where we treat the data pointer
        /// as a plain `usize` and store the bits of self in it.
        fn create_thin<W: Wake>(wake: W) -> RawWaker {
            let mut data = ptr::null();

            // The following code will unleash the kraken if this invariant
            // isn't upheld.
            debug_assert!(mem::size_of::<W>() <= mem::size_of_val(&data));

            // The size of `W` might be _smaller_ than a pointer, so we can't
            // simply transmute here as that would potentially read off the end
            // of `wake`. Instead, we copy from `wake` to `data` (not the
            // _target_ of `data`, which has no meaning to us).
            unsafe {
                ptr::copy_nonoverlapping(
                    &wake as *const W,
                    &mut data as *mut *const () as *mut W,
                    1,
                );
            }

            // We moved `wake` into `data`, so make sure we don't keep the old
            // copy around (there can be only one!).
            mem::forget(wake);

            RawWaker::new(
                data,
                &RawWakerVTable::new(
                    |data| unsafe {
                        create_raw_waker((&*(&data as *const *const () as *const W)).clone())
                    },
                    |data| unsafe {
                        mem::transmute_copy::<_, W>(&data).wake();
                    },
                    |data| unsafe {
                        (&*(&data as *const *const () as *const W)).wake_by_ref();
                    },
                    |data| unsafe {
                        mem::transmute_copy::<_, W>(&data);
                    },
                ),
            )
        }

        unsafe { Waker::from_raw(create_raw_waker(self)) }
    }
}

impl Wake for std::thread::Thread {
    fn wake_by_ref(&self) {
        self.unpark();
    }
}

/// Create a waker from a closure.
///
/// # Examples
///
/// ```
/// let waker = wakeful::waker_fn(move || {
///     println!("time for work!");
/// });
///
/// waker.wake();
/// ```
pub fn waker_fn(f: impl Fn() + Send + Sync + 'static) -> Waker {
    struct Impl<F>(Arc<F>);

    impl<F> Clone for Impl<F> {
        fn clone(&self) -> Self {
            Impl(self.0.clone())
        }
    }

    impl<F: Fn() + Send + Sync + 'static> Wake for Impl<F> {
        fn wake_by_ref(&self) {
            (self.0)()
        }
    }

    Impl(Arc::new(f)).into_waker()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn zero_sized_impl() {
        static WOKE: AtomicUsize = AtomicUsize::new(0);

        #[derive(Clone)]
        struct Impl;

        impl Wake for Impl {
            fn wake_by_ref(&self) {
                WOKE.fetch_add(1, Ordering::SeqCst);
            }
        }

        let waker = Impl.into_waker();
        waker.wake_by_ref();
        assert_eq!(WOKE.load(Ordering::SeqCst), 1);

        waker.clone().wake();
        assert_eq!(WOKE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn ptr_sized_impl() {
        #[derive(Clone, Default)]
        struct Impl(Arc<AtomicUsize>);

        impl Wake for Impl {
            fn wake_by_ref(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let woke = Arc::new(AtomicUsize::new(0));

        let waker = Impl(woke.clone()).into_waker();
        waker.wake_by_ref();
        assert_eq!(woke.load(Ordering::SeqCst), 1);

        waker.clone().wake();
        assert_eq!(woke.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn bigger_than_ptr_sized_impl() {
        #[derive(Clone)]
        struct Impl(Arc<AtomicUsize>, usize);

        impl Wake for Impl {
            fn wake_by_ref(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let woke = Arc::new(AtomicUsize::new(0));

        let waker = Impl(woke.clone(), 0).into_waker();
        waker.wake_by_ref();
        assert_eq!(woke.load(Ordering::SeqCst), 1);

        waker.clone().wake();
        assert_eq!(woke.load(Ordering::SeqCst), 2);
    }
}
