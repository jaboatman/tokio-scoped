extern crate futures;
extern crate tokio;

use futures::sync::mpsc;
use futures::{Future, Stream};
use std::mem::ManuallyDrop;
use std::ops::Deref;

pub struct ScopedRuntime {
    rt: tokio::runtime::Runtime,
}

pub struct Scope<'a> {
    t: &'a mut ScopedRuntime,
    send: ManuallyDrop<mpsc::Sender<()>>,
    recv: Option<mpsc::Receiver<()>>,
}

impl ScopedRuntime {
    pub fn new(rt: tokio::runtime::Runtime) -> Self {
        ScopedRuntime { rt }
    }
    pub fn scope<'a, F, R>(&'a mut self, f: F) -> R
    where
        F: FnOnce(&mut Scope<'a>) -> R,
    {
        let (s, r) = mpsc::channel(0);
        let mut s = Scope {
            t: self,
            send: ManuallyDrop::new(s),
            recv: Some(r),
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
    pub fn spawn<'s, F>(&'s mut self, future: F) -> &Self
    where
        F: Future<Item = (), Error = ()> + Send + 'a,
        'a: 's,
    {
        let scoped_f = self.scoped_future(future);
        self.t.rt.spawn(scoped_f);
        self
    }

    pub fn block_on<'s, R, E, F>(&'s mut self, future: F) -> Result<R, E>
    where
        F: Future<Item = R, Error = E> + Send + 'a,
        R: Send + 'static,
        E: Send + 'static,
        'a: 's,
    {
        let boxed: Box<Future<Item = R, Error = E> + Send + 'a> = Box::new(future);
        let boxed: Box<Future<Item = R, Error = E> + Send + 'static> =
            unsafe { std::mem::transmute(boxed) };

        self.t.rt.block_on(boxed)
    }
}

impl<'a> Drop for Scope<'a> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.send);
        }

        self.t
            .rt
            .block_on(self.recv.take().unwrap().for_each(|_| futures::empty()))
            .ok();
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
        let mut scoped = make_runtime();
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
        let mut scoped = make_runtime();
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
        let mut scoped = make_runtime();
        let mut uncopy = String::from("Borrowed");
        let mut uncopy2 = String::from("Borrowed");
        scoped.scope(|scope| {
            scope.spawn(lazy(|| {
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
        let mut scoped = make_runtime();
        let uncopy = String::from("Borrowed");
        let captured = scoped.scope(|scope| {
            let v = scope
                .block_on(lazy(|| {
                    let mut v = uncopy.clone();
                    v.push('!');
                    Ok::<_, ()>(v)
                })).unwrap();
            assert_eq!(v.as_str(), "Borrowed!");
            v
        });
        assert_eq!(captured.as_str(), "Borrowed!");
    }
}
