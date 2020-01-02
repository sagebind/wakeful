use std::{
    mem,
    ptr,
    task::{RawWaker, RawWakerVTable, Waker},
};

/// Create a waker from a closure.
pub fn waker_fn(f: impl Fn() + Send + Sync + 'static) -> Waker {
    use std::sync::Arc;

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

/// Helper trait that makes it easier to implement wakers.
///
/// Implementing this trait provides you with [`Wake::into_waker`], which allows
/// you to construct a [`Waker`] from any type implementing [`Wake`].
///
/// If the size of `Self` is less than or equal to pointer size, as an
/// optimization the underlying implementation will pass `self` in directly to
/// `RawWakerVTable` functions. For types larger than a pointer, an allocation
/// will be made on creation and when cloning.
pub trait Wake: Send + Sync + Clone {
    /// Wake up the task associated with this waker, consuming the waker.
    ///
    /// By default, this delegates to [`Wake::wake_by_ref`], but can be
    /// overridden if a more efficient implementation is possible.
    fn wake(self) {
        self.wake_by_ref();
    }

    /// Wake up the task associated with this waker, consuming the waker.
    fn wake_by_ref(&self);

    /// Convert this into a [`Waker`] handle.
    fn into_waker(self) -> Waker {
        unsafe { Waker::from_raw(self.into_raw_waker()) }
    }

    /// Convert this into a [`RawWaker`] handle.
    #[inline]
    fn into_raw_waker(self) -> RawWaker {
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

        /// Convert a wake into a [`RawWaker`] handle by allocating a box.
        fn into_boxed<W: Wake>(wake: W) -> RawWaker {
            RawWaker::new(
                Box::into_raw(Box::new(wake)) as *const (),
                &RawWakerVTable::new(
                    |data| unsafe { (&*(data as *const W)).clone().into_raw_waker() },
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
        fn into_thin<W: Wake>(wake: W) -> RawWaker {
            let mut data = std::ptr::null();

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
                        (&*(&data as *const *const () as *const W))
                            .clone()
                            .into_raw_waker()
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

        if mem::size_of::<Self>() <= mem::size_of::<*const ()>() {
            into_thin(self)
        } else {
            into_boxed(self)
        }
    }
}

impl Wake for std::thread::Thread {
    fn wake_by_ref(&self) {
        self.unpark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
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
