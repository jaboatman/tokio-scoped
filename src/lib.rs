//! A scoped [`tokio`] Runtime that can be used to create [`Scope`]s which can spawn futures which
//! can access stack data. That is, the futures spawned by the [`Scope`] do not require the `'static`
//! lifetime bound. This can be done safely by ensuring that the [`Scope`] doesn't exit until all
//! spawned futures have finished executing. Be aware, that when a [`Scope`] exits it will block
//! until every future spawned by the [`Scope`] completes. Therefore, one should take caution when
//! created scopes within an asynchronous context, such as from within another spawned future.
//!
//! # Example
//! ```
//! # extern crate tokio_scoped;
//! # extern crate futures;
//! # use futures::lazy;
//!
//! let mut v = String::from("Hello");
//! tokio_scoped::scope(|scope| {
//!     // Use the scope to spawn the future.
//!     scope.spawn(lazy(|| {
//!         v.push('!');
//!         Ok(())
//!     }));
//! });
//! // The scope won't exit until all spawned futures are complete.
//! assert_eq!(v.as_str(), "Hello!");
//! ```
//!
//! See also [`crossbeam::scope`]
//!
//! [`tokio`]: https://tokio.rs/
//! [`crossbeam::scope`]: https://docs.rs/crossbeam/0.4.1/crossbeam/fn.scope.html
extern crate futures;
extern crate tokio;

use futures::sync::mpsc;
use futures::{Future, Stream};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use tokio::runtime::TaskExecutor;

/// Creates a [`ScopedRuntime`] and calls the `scope` method with `f` on it.
///
/// # Example
/// ```
/// # extern crate tokio_scoped;
/// # extern crate futures;
/// # use futures::lazy;
///
/// let mut v = String::from("Hello");
/// tokio_scoped::scope(|scope| {
///     // Use the scope to spawn the future.
///     scope.spawn(lazy(|| {
///         v.push('!');
///         Ok(())
///     }));
/// });
/// // The scope won't exit until all spawned futures are complete.
/// assert_eq!(v.as_str(), "Hello!");
/// ```
pub fn scope<'a, F, R>(f: F) -> R
where
    F: FnOnce(&mut Scope<'a>) -> R,
{
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut scope = Scope::new(rt.executor());
    f(&mut scope)
}

/// Borrows the Runtime to construct a `ScopeBuilder` which can be used to create a scope.
///
/// # Example
/// ```
/// # extern crate tokio_scoped;
/// # extern crate futures;
/// # extern crate tokio;
/// # use futures::lazy;
///
/// let mut v = String::from("Hello");
/// let rt = tokio::runtime::Runtime::new().unwrap();
/// tokio_scoped::scoped(&rt).scope(|scope| {
///     // Use the scope to spawn the future.
///     scope.spawn(lazy(|| {
///         v.push('!');
///         Ok(())
///     }));
/// });
/// // The scope won't exit until all spawned futures are complete.
/// assert_eq!(v.as_str(), "Hello!");
/// ```
pub fn scoped(rt: &tokio::runtime::Runtime) -> ScopeBuilder<'_> {
    ScopeBuilder::from_runtime(rt)
}

/// Wrapper type around a tokio Runtime which can be used to create `Scope`s. This type takes
/// ownership of the Runtime. For an alternative approach that
///
/// See also the [`scope`] function.
///
/// [`scope`]: /tokio-scoped/fn.scope.html
#[derive(Debug)]
pub struct ScopedRuntime {
    rt: tokio::runtime::Runtime,
}

/// Struct used to build scopes from a borrowed Runtime. Generally users should use the [`scoped`]
/// function instead of building `ScopeBuilder` instances directly.
///
/// [`scoped`]: /tokio-scoped/fn.scoped.html
#[derive(Debug)]
pub struct ScopeBuilder<'a> {
    rt: &'a tokio::runtime::Runtime,
}

#[derive(Debug)]
pub struct Scope<'a> {
    exec: TaskExecutor,
    send: ManuallyDrop<mpsc::Sender<()>>,
    // When the `Scope` is dropped, we wait on this receiver to close. No messages are sent through
    // the receiver, however, the `Sender` objects get cloned into each spawned future (see
    // `ScopedFuture`). This is how we ensure they all exit eventually.
    recv: Option<mpsc::Receiver<()>>,
    _marker: PhantomData<&'a ()>,
}

impl<'a> Scope<'a> {
    fn new(exec: TaskExecutor) -> Scope<'a> {
        let (s, r) = mpsc::channel(0);
        Scope {
            exec,
            send: ManuallyDrop::new(s),
            recv: Some(r),
            _marker: PhantomData,
        }
    }
}

impl ScopedRuntime {
    pub fn new(rt: tokio::runtime::Runtime) -> Self {
        ScopedRuntime { rt }
    }

    /// Creates a scope bound by the lifetime of `self` that can be used to spawn scoped futures.
    pub fn scope<'a, F, R>(&'a self, f: F) -> R
    where
        F: FnOnce(&mut Scope<'a>) -> R,
    {
        let mut scope = Scope::new(self.rt.executor());
        f(&mut scope)
    }

    /// Consumes the `ScopedRuntime` and returns the inner [`Runtime`] variable.
    ///
    /// [`Runtime`]: https://docs.rs/tokio/0.1.8/tokio/runtime/struct.Runtime.html
    pub fn into_inner(self) -> tokio::runtime::Runtime {
        self.rt
    }
}

impl<'a> ScopeBuilder<'a> {
    pub fn from_runtime(rt: &'a tokio::runtime::Runtime) -> ScopeBuilder<'a> {
        ScopeBuilder { rt }
    }

    pub fn scope<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Scope<'a>) -> R,
    {
        let mut scope = Scope::new(self.rt.executor());
        f(&mut scope)
    }
}

struct ScopedFuture {
    f: Box<Future<Item = (), Error = ()> + Send + 'static>,
    _marker: mpsc::Sender<()>,
}

impl Future for ScopedFuture {
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> futures::Poll<(), ()> {
        self.f.poll()
    }
}

impl<'a> Scope<'a> {
    fn scoped_future<'s, F>(&'s self, f: F) -> ScopedFuture
    where
        F: Future<Item = (), Error = ()> + Send + 'a,
        'a: 's,
    {
        let boxed: Box<Future<Item = (), Error = ()> + Send + 'a> = Box::new(f);
        // This transmute should be safe, as we use the `ScopedFuture` abstraction to prevent the
        // scope from exiting until every spawned `ScopedFuture` object is dropped, signifying that
        // they have completed their execution.
        let boxed: Box<Future<Item = (), Error = ()> + Send + 'static> =
            unsafe { std::mem::transmute(boxed) };
        ScopedFuture {
            f: boxed,
            _marker: self.send.deref().clone(),
        }
    }

    /// Spawn the received future on the [`ScopedRuntime`] which was used to create `self`.
    ///
    /// [`ScopedRuntime`]: /tokio-scoped/struct.ScopedRuntime.html
    pub fn spawn<'s, F>(&'s mut self, future: F) -> &mut Self
    where
        F: Future<Item = (), Error = ()> + Send + 'a,
        'a: 's,
    {
        let scoped_f = self.scoped_future(future);
        self.exec.spawn(scoped_f);
        self
    }

    /// Blocks the "current thread" of the runtime until `future` resolves. Other spawned futures
    /// can make progress while this future is running.
    pub fn block_on<'s, R, E, F>(&'s mut self, future: F) -> Result<R, E>
    where
        F: Future<Item = R, Error = E> + Send + 'a,
        R: Send + 'a,
        E: Send + 'a,
        'a: 's,
    {
        let (tx, rx) = futures::sync::oneshot::channel();
        let future = future.then(move |r| tx.send(r).map_err(|_| unreachable!()));
        let boxed: Box<Future<Item = (), Error = ()> + Send + 'a> = Box::new(future);
        let boxed: Box<Future<Item = (), Error = ()> + Send + 'static> =
            unsafe { std::mem::transmute(boxed) };
        self.exec.spawn(boxed);
        rx.wait().unwrap()
    }

    /// Creates an `inner` scope which can access variables created within the outer scope.
    pub fn scope<'inner, F, R>(&'inner self, f: F) -> R
    where
        F: FnOnce(&mut Scope<'inner>) -> R,
        'a: 'inner,
    {
        let mut scope = Scope::new(self.exec.clone());
        f(&mut scope)
    }

    /// Get a `TaskExecutor` to the underlying `Runtime` instance.
    pub fn executor(&self) -> TaskExecutor {
        self.exec.clone()
    }
}

impl<'a> Drop for Scope<'a> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.send);
        }

        let recv = self.recv.take().unwrap();
        let n = recv.wait().next();
        assert_eq!(n, None);
    }
}

#[cfg(test)]
mod testing {
    use super::*;
    use futures::lazy;
    use std::{thread, time::Duration};
    use tokio;
    fn make_runtime() -> ScopedRuntime {
        let rt = tokio::runtime::Runtime::new().expect("Failed to construct Runtime");
        ScopedRuntime::new(rt)
    }

    #[test]
    fn basic_test() {
        let scoped = make_runtime();
        scoped.scope(|scope| {
            scope.spawn(lazy(|| {
                tokio::spawn(lazy(|| {
                    println!("Another!");
                    thread::sleep(Duration::from_millis(5000));
                    println!("Another is done sleeping");
                    Ok(())
                }));

                println!("Sleeping a spawned future");
                // We should be able to spawn more and also verify that they complete...
                thread::sleep(Duration::from_millis(2000));
                println!("Completing!");
                Ok(())
            }));
        });
        println!("Completed");
    }

    #[test]
    fn access_stack() {
        let scoped = make_runtime();
        // Specifically a variable that does _not_ implement Copy.
        let uncopy = String::from("Borrowed!");
        scoped.scope(|scope| {
            scope.spawn(lazy(|| {
                assert_eq!(uncopy.as_str(), "Borrowed!");
                println!("Borrowed successfully: {}", uncopy);
                Ok(())
            }));
        });
    }

    #[test]
    fn access_mut_stack() {
        let scoped = make_runtime();
        let mut uncopy = String::from("Borrowed");
        let mut uncopy2 = String::from("Borrowed");
        scoped.scope(|scope| {
            scope.spawn(lazy(|| {
                let f = scoped
                    .scope(|scope2| scope2.block_on(lazy(|| Ok::<_, ()>(4))))
                    .unwrap();
                assert_eq!(f, 4);
                thread::sleep(Duration::from_millis(1000));
                uncopy.push('!');
                Ok(())
            }));

            scope.spawn(lazy(|| {
                uncopy2.push('f');
                Ok(())
            }));
        });

        assert_eq!(uncopy.as_str(), "Borrowed!");
        assert_eq!(uncopy2.as_str(), "Borrowedf");
    }

    #[test]
    fn access_mut_stack_scope_fn() {
        let mut uncopy = String::from("Borrowed");
        let mut uncopy2 = String::from("Borrowed");
        ::scope(|scope| {
            scope.spawn(lazy(|| {
                uncopy.push('!');
                Ok(())
            }));

            scope.spawn(lazy(|| {
                uncopy2.push('f');
                Ok(())
            }));
        });

        assert_eq!(uncopy.as_str(), "Borrowed!");
        assert_eq!(uncopy2.as_str(), "Borrowedf");
    }

    #[test]
    fn block_on_test() {
        let scoped = make_runtime();
        let mut uncopy = String::from("Borrowed");
        let captured = scoped.scope(|scope| {
            let v = scope
                .block_on(lazy(|| {
                    uncopy.push('!');
                    Ok::<_, ()>(uncopy)
                }))
                .unwrap();
            assert_eq!(v.as_str(), "Borrowed!");
            v
        });
        assert_eq!(captured.as_str(), "Borrowed!");
    }

    #[test]
    fn borrow_many_test() {
        let scoped = make_runtime();
        let mut values = vec![1, 2, 3, 4];
        scoped.scope(|scope| {
            for v in &mut values {
                scope.spawn(lazy(move || {
                    *v += 1;
                    Ok(())
                }));
            }
        });

        assert_eq!(&values, &[2, 3, 4, 5]);
    }

    #[test]
    fn inner_scope_test() {
        let scoped = make_runtime();
        let mut values = vec![1, 2, 3, 4];
        scoped.scope(|scope| {
            let mut v2s = vec![2, 3, 4, 5];
            scope.scope(|scope2| {
                scope2.spawn(lazy(|| {
                    v2s.push(100);
                    values.push(100);
                    Ok(())
                }));
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
        ::scoped(&rt).scope(|scope| {
            scope.spawn(lazy(|| {
                values.push(100);
                Ok(())
            }));
        });
        assert_eq!(values, &[1, 2, 3, 4, 100]);
    }
}
