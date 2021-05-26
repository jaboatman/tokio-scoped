//! A scoped [`tokio`] Runtime that can be used to create [`Scope`]s which can spawn futures which
//! can access stack data. That is, the futures spawned by the [`Scope`] do not require the `'static`
//! lifetime bound. This can be done safely by ensuring that the [`Scope`] doesn't exit until all
//! spawned futures have finished executing. Be aware, that when a [`Scope`] exits it will block
//! until every future spawned by the [`Scope`] completes. Therefore, one should take caution when
//! created scopes within an asynchronous context, such as from within another spawned future.
//!
//! # Example
//! ```
//! #[tokio::main]
//! async fn main() {
//!     let mut v = String::from("Hello");
//!     tokio_scoped::scope(|scope| {
//!         // Use the scope to spawn the future.
//!         scope.spawn(async {
//!             v.push('!');
//!         });
//!     });
//!     // The scope won't exit until all spawned futures are complete.
//!     assert_eq!(v.as_str(), "Hello!");
//! }
//! ```
//!
//! See also [`crossbeam::scope`]
//!
//! [`tokio`]: https://tokio.rs/
//! [`crossbeam::scope`]: https://docs.rs/crossbeam/0.4.1/crossbeam/fn.scope.html

use std::{
    borrow::Cow,
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::Deref,
    pin::Pin,
    task::{Context, Poll}
};

use futures::{FutureExt, StreamExt};
use futures::channel::mpsc;
use tokio::runtime::Handle;

/// Creates a [`Scope`] using the current tokio runtime and calls the `scope` method with the
/// provided future
///
/// # Example
/// ```
/// #[tokio::main]
/// async fn main() {
///     let mut v = String::from("Hello");
///     tokio_scoped::scope(|scope| {
///         // Use the scope to spawn the future.
///         scope.spawn(async {
///             v.push('!');
///         });
///     });
///     // The scope won't exit until all spawned futures are complete.
///     assert_eq!(v.as_str(), "Hello!");
/// }
/// ```
pub fn scope<'a, F, R>(f: F) -> R
where
    F: FnOnce(&mut Scope<'a>) -> R,
{
    let mut scope = Scope::new(Cow::Owned(Handle::current()));
    f(&mut scope)
}

/// Borrows a `Handle` to the tokio `Runtime` to construct a [`ScopeBuilder`] which can be used to
/// create a scope.
///
/// # Example
/// ```
/// let mut v = String::from("Hello");
/// let rt = tokio::runtime::Runtime::new().unwrap();
/// tokio_scoped::scoped(rt.handle()).scope(|scope| {
///     // Use the scope to spawn the future.
///     scope.spawn(async {
///         v.push('!');
///     });
/// });
/// // The scope won't exit until all spawned futures are complete.
/// assert_eq!(v.as_str(), "Hello!");
/// ```
pub fn scoped(tokio_handle: &Handle) -> ScopeBuilder<'_> {
    ScopeBuilder { handle: tokio_handle }
}

/// Struct used to build scopes from a borrowed `Handle`. Generally users should use the [`scoped`]
/// function instead of building `ScopeBuilder` instances directly.
///
/// [`scoped`]: /tokio-scoped/fn.scoped.html
#[derive(Debug)]
pub struct ScopeBuilder<'a> {
    handle: &'a Handle,
}

#[derive(Debug)]
pub struct Scope<'a> {
    handle: Cow<'a, Handle>,
    send: ManuallyDrop<mpsc::Sender<()>>,
    // When the `Scope` is dropped, we wait on this receiver to close. No messages are sent through
    // the receiver, however, the `Sender` objects get cloned into each spawned future (see
    // `ScopedFuture`). This is how we ensure they all exit eventually.
    recv: Option<mpsc::Receiver<()>>,
    _marker: PhantomData<&'a ()>,
}

impl<'a> Scope<'a> {
    fn new<'b: 'a>(handle: Cow<'b, Handle>) -> Scope<'a> {
        let (s, r) = mpsc::channel(0);
        Scope {
            handle,
            send: ManuallyDrop::new(s),
            recv: Some(r),
            _marker: PhantomData,
        }
    }
}

impl<'a> ScopeBuilder<'a> {
    pub fn from_runtime(rt: &'a tokio::runtime::Runtime) -> ScopeBuilder<'a> {
        ScopeBuilder { handle: rt.handle() }
    }

    pub fn scope<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Scope<'a>) -> R,
    {
        let mut scope = Scope::new(Cow::Borrowed(self.handle));
        f(&mut scope)
    }
}

struct ScopedFuture {
    f: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    _marker: mpsc::Sender<()>,
}

impl Future for ScopedFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner_future = &mut self.f;
        let future_ref = Pin::as_mut(inner_future);
        future_ref.poll(cx)
    }
}

impl<'a> Scope<'a> {
    fn scoped_future<'s, F>(&'s self, f: F) -> ScopedFuture
    where
        F: Future<Output = ()> + Send + 'a,
        'a: 's,
    {
        let boxed: Pin<Box<dyn Future<Output = ()> + Send + 'a>> = Box::pin(f);
        // This transmute should be safe, as we use the `ScopedFuture` abstraction to prevent the
        // scope from exiting until every spawned `ScopedFuture` object is dropped, signifying that
        // they have completed their execution.
        let boxed: Pin<Box<dyn Future<Output = ()> + Send + 'static>> =
            unsafe { std::mem::transmute(boxed) };

        ScopedFuture {
            f: boxed,
            _marker: self.send.deref().clone(),
        }
    }

    /// Spawn the provided future on the `Handle` to the tokio `Runtime`.
    pub fn spawn<'s, F>(&'s mut self, future: F) -> &mut Self
    where
        F: Future<Output = ()> + Send + 'a,
        'a: 's,
    {
        let scoped_f = self.scoped_future(future);
        self.handle.spawn(scoped_f);
        self
    }

    /// Creates an `inner` scope which can access variables created within the outer scope.
    pub fn scope<'inner, F, R>(&'inner self, f: F) -> R
    where
        F: FnOnce(&mut Scope<'inner>) -> R,
        'a: 'inner,
    {
        let mut scope = Scope::new(self.handle.clone());
        f(&mut scope)
    }

    /// Get a `Handle` to the underlying `Runtime` instance.
    pub fn handle(&self) -> Handle {
        (&*self.handle).clone()
    }
}

impl<'a> Drop for Scope<'a> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.send);
        }

        let mut recv = self.recv.take().unwrap();
        let n = futures::executor::block_on(recv.next());
        assert_eq!(n, None);
    }
}

#[cfg(test)]
mod testing {
    use super::*;
    use std::{thread, time::Duration};
    use tokio::runtime::Runtime;

    fn make_runtime() -> Runtime {
        Runtime::new().expect("Failed to construct Runtime")
    }

    #[test]
    fn basic_test() {
        let rt = make_runtime();
        let scoped = scoped(rt.handle());
        scoped.scope(|scope| {
            scope.spawn(async {
                tokio::spawn(async {
                    println!("Another!");
                    thread::sleep(Duration::from_millis(5000));
                    println!("Another is done sleeping");
                });

                println!("Sleeping a spawned future");
                // We should be able to spawn more and also verify that they complete...
                thread::sleep(Duration::from_millis(2000));
                println!("Completing!");
            });
        });
        println!("Completed");
    }

    #[test]
    fn access_stack() {
        let rt = make_runtime();
        let scoped = scoped(rt.handle());
        // Specifically a variable that does _not_ implement Copy.
        let uncopy = String::from("Borrowed!");
        scoped.scope(|scope| {
            scope.spawn(async {
                assert_eq!(uncopy.as_str(), "Borrowed!");
                println!("Borrowed successfully: {}", uncopy);
            });
        });
    }

    #[test]
    fn access_mut_stack() {
        let rt = make_runtime();
        let scoped = scoped(rt.handle());
        let mut uncopy = String::from("Borrowed");
        let mut uncopy2 = String::from("Borrowed");
        scoped.scope(|scope| {
            scope.spawn(async {
                let f = scoped.scope(|scope2| async { 4 }).await;
                assert_eq!(f, 4);
                thread::sleep(Duration::from_millis(1000));
                uncopy.push('!');
            });

            scope.spawn(async {
                uncopy2.push('f');
            });
        });

        assert_eq!(uncopy.as_str(), "Borrowed!");
        assert_eq!(uncopy2.as_str(), "Borrowedf");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn access_mut_stack_scope_fn() {
        let mut uncopy = String::from("Borrowed");
        let mut uncopy2 = String::from("Borrowed");
        scope(|scope| {
            scope.spawn(async {
                uncopy.push('!');
            });

            scope.spawn(async {
                uncopy2.push('f');
            });
        });

        assert_eq!(uncopy.as_str(), "Borrowed!");
        assert_eq!(uncopy2.as_str(), "Borrowedf");
    }

    #[test]
    fn borrow_many_test() {
        let rt = make_runtime();
        let scoped = scoped(rt.handle());
        let mut values = vec![1, 2, 3, 4];
        scoped.scope(|scope| {
            for v in &mut values {
                scope.spawn(async move {
                    *v += 1;
                });
            }
        });

        assert_eq!(&values, &[2, 3, 4, 5]);
    }

    #[test]
    fn inner_scope_test() {
        let rt = make_runtime();
        let scoped = scoped(rt.handle());
        let mut values = vec![1, 2, 3, 4];
        scoped.scope(|scope| {
            let mut v2s = vec![2, 3, 4, 5];
            scope.scope(|scope2| {
                scope2.spawn(async {
                    v2s.push(100);
                    values.push(100);
                });
            });
            // The inner scope must exit before we can get here.
            assert_eq!(v2s, &[2, 3, 4, 5, 100]);
            assert_eq!(values, &[1, 2, 3, 4, 100]);
        });
    }

    #[test]
    fn borrowed_scope_test() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut values = vec![1, 2, 3, 4];
        ScopeBuilder::from_runtime(&rt).scope(|scope| {
            scope.spawn(async {
                values.push(100);
            });
        });
        assert_eq!(values, &[1, 2, 3, 4, 100]);
    }
}
