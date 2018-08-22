extern crate futures;
extern crate tokio;

use futures::sync::mpsc;
use futures::{Future, Stream};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use tokio::runtime::TaskExecutor;

pub struct ScopedRuntime {
    rt: tokio::runtime::Runtime,
}

pub struct Scope<'a> {
    exec: TaskExecutor,
    send: ManuallyDrop<mpsc::Sender<()>>,
    recv: Option<mpsc::Receiver<()>>,
    _marker: PhantomData<&'a ()>,
}

impl ScopedRuntime {
    pub fn new(rt: tokio::runtime::Runtime) -> Self {
        ScopedRuntime { rt }
    }
    pub fn scope<'a, F, R>(&'a self, f: F) -> R
    where
        F: FnOnce(&mut Scope<'a>) -> R,
    {
        let (s, r) = mpsc::channel(0);
        let mut s = Scope {
            exec: self.rt.executor(),
            send: ManuallyDrop::new(s),
            recv: Some(r),
            _marker: PhantomData,
        };
        f(&mut s)
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
        let boxed: Box<Future<Item = (), Error = ()> + Send + 'static> =
            unsafe { std::mem::transmute(boxed) };
        ScopedFuture {
            f: boxed,
            _marker: self.send.deref().clone(),
        }
    }
    pub fn spawn<'s, F>(&'s mut self, future: F) -> &mut Self
    where
        F: Future<Item = (), Error = ()> + Send + 'a,
        'a: 's,
    {
        let scoped_f = self.scoped_future(future);
        self.exec.spawn(scoped_f);
        self
    }

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
}

impl<'a> Drop for Scope<'a> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.send);
        }

        let recv = self.recv.take().unwrap();
        self.block_on(recv.for_each(|_| futures::empty())).ok();
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
    fn block_on_test() {
        let scoped = make_runtime();
        let mut uncopy = String::from("Borrowed");
        let captured = scoped.scope(|scope| {
            let v = scope
                .block_on(lazy(|| {
                    uncopy.push('!');
                    Ok::<_, ()>(uncopy)
                })).unwrap();
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
}
