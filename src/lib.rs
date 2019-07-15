//! This crate provides a thread pool for executing short, I/O-heavy futures efficiently.
//!
//! The standard `Runtime` provided by `tokio` uses a thread-pool to allow concurrent execution of
//! compute-heavy futures. However, its work-stealing makes it so that futures may be executed on
//! different threads to where their reactor are running, which results in unnecessary
//! synchronization, and thus lowers the achievable throughput. While this trade-off works well for
//! many asynchronous applications, since it spreads load more evenly, it is not a great fit for
//! high-performance I/O bound applications where the cost of synchronizing threads is high. This
//! can happen, for example, if your application performs frequent but small I/O operations.
//!
//! This crate provides an alternative implementation of a futures-based thread pool. It spawns a
//! pool of threads that each runs a `tokio::runtime::current_thread::Runtime` (and thus each have
//! an I/O reactor of their own), and spawns futures onto the pool by assigning the future to
//! threads round-robin. Once a future has been spawned onto a thread, it, and any child futures it
//! may produce through `tokio::spawn`, remain under the control of that same thread.
//!
//! In general, you should use `tokio-io-pool` only if you perform a lot of very short I/O
//! operations on many futures, and find that you are bottlenecked by work-stealing or reactor
//! notifications with the regular `tokio` runtime. If you are unsure what to use, start with the
//! `tokio` runtime.
//!
//! Be aware that this pool does *not* support the
//! [`blocking`](https://docs.rs/tokio-threadpool/0.1.5/tokio_threadpool/fn.blocking.html) function
//! since it is [not supported](https://github.com/tokio-rs/tokio/issues/432) by the underlying
//! `current_thread::Runtime`. Hopefully this will be rectified down the line.
//!
//! There is some discussion around trying to merge this pool into `tokio` proper; that effort is
//! tracked in [tokio-rs/tokio#486](https://github.com/tokio-rs/tokio/issues/486).
//!
//! # Examples
//!
//! ```no_run
//! #![feature(async_await)]
//! use tokio::prelude::*;
//! use tokio::io::AsyncReadExt;
//! use tokio::net::TcpListener;
//!
//! fn main() {
//!     // Bind the server's socket.
//!     let addr = "127.0.0.1:12345".parse().unwrap();
//!     let mut listener = TcpListener::bind(&addr).expect("unable to bind TCP listener");
//!
//!     // Pull out a stream of sockets for incoming connections
//!     let server = async move {
//!         loop {
//!             let (sock, _) = listener.accept().await.expect("acccept failed");
//!             tokio::spawn(async move {
//!                 let (mut reader, mut writer) = sock.split();
//!                 let bytes_copied = reader.copy(&mut writer);
//!                 let n = bytes_copied.await.expect("I/O error");
//!                 println!("wrote {} bytes", n);
//!             });
//!         }
//!     };
//!
//!     // Start the Tokio runtime
//!     tokio_io_pool::run(server);
//! }
//! ```

#![deny(missing_docs)]
#![deny(missing_debug_implementations)]
#![deny(missing_copy_implementations)]
#![deny(unused_extern_crates)]
#![feature(async_await)]

use futures_core::{
    future::{Future},
    ready,
    stream::TryStream,
    task::{Context, Poll},
};
use std::pin::Pin;
use std::marker::Unpin;
use std::sync::{atomic, mpsc, Arc};
use std::{fmt, io, thread};
use tokio::executor::SpawnError;
use tokio::runtime::current_thread;
use tokio::sync::oneshot;

/// Builds an I/O-oriented thread pool ([`Runtime`]) with custom configuration values.
///
/// Methods can be chained in order to set the configuration values. The thread pool is constructed
/// by calling [`Builder::build`]. New instances of `Builder` are obtained via
/// [`Builder::default`].
///
/// See function level documentation for details on the various configuration settings.
pub struct Builder {
    nworkers: usize,
    name_prefix: Option<String>,
    after_start: Option<Arc<dyn Fn() + Send + Sync>>,
    before_stop: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for Builder {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Builder")
            .field("nworkers", &self.nworkers)
            .field("name_prefix", &self.name_prefix)
            .finish()
    }
}

impl Default for Builder {
    fn default() -> Self {
        Builder {
            nworkers: num_cpus::get(),
            name_prefix: None,
            after_start: None,
            before_stop: None,
        }
    }
}

impl Builder {
    /// Set the number of worker threads for the thread pool instance.
    ///
    /// This must be a number between 1 and 32,768 though it is advised to keep
    /// this value on the smaller side.
    ///
    /// The default value is the number of cores available to the system.
    pub fn pool_size(&mut self, val: usize) -> &mut Self {
        self.nworkers = val;
        self
    }

    /// Set name prefix of threads spawned by the scheduler
    ///
    /// Thread name prefix is used for generating thread names. For example, if prefix is
    /// `my-pool-`, then threads in the pool will get names like `my-pool-1` etc.
    ///
    /// If this configuration is not set, then the thread will use the system default naming
    /// scheme.
    pub fn name_prefix<S: Into<String>>(&mut self, val: S) -> &mut Self {
        self.name_prefix = Some(val.into());
        self
    }

    /// Execute function `f` after each thread is started but before it starts doing work.
    ///
    /// This is intended for bookkeeping and monitoring use cases.
    pub fn after_start<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.after_start = Some(Arc::new(f));
        self
    }

    /// Execute function `f` before each thread stops.
    ///
    /// This is intended for bookkeeping and monitoring use cases.
    pub fn before_stop<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.before_stop = Some(Arc::new(f));
        self
    }

    /// Create the configured [`Runtime`].
    ///
    /// The returned [`Runtime`] instance is ready to spawn tasks.
    pub fn build(&self) -> io::Result<Runtime> {
        assert!(self.nworkers > 0);

        let mut handles = Vec::with_capacity(self.nworkers);
        let mut threads = Vec::with_capacity(self.nworkers);
        for i in 0..self.nworkers {
            let (trigger, exit) = oneshot::channel();
            let (handle_tx, handle_rx) = mpsc::sync_channel(1);

            let mut th = thread::Builder::new();

            if let Some(ref prefix) = self.name_prefix {
                th = th.name(format!("{}{}", prefix, i + 1));
            }

            let before = self.after_start.clone();
            let after = self.before_stop.clone();

            let jh = th.spawn(move || {
                if let Some(ref f) = before {
                    f();
                }

                let mut rt = current_thread::Runtime::new().unwrap();
                handle_tx.send(rt.handle()).unwrap();
                let force_exit: bool = rt.block_on(exit).unwrap();
                if !force_exit {
                    rt.run().unwrap();
                }

                if let Some(ref f) = after {
                    f();
                }
            })?;

            threads.push((trigger, jh));
            handles.push(handle_rx.recv().unwrap());
        }

        let handle = Handle {
            workers: handles,
            rri: Arc::new(atomic::AtomicUsize::new(0)),
        };

        Ok(Runtime {
            threads,
            force_exit: true,
            handle,
        })
    }
}

/// Execute the given future and spawn any child futures onto a newly created I/O thread pool.
///
/// This function is used to bootstrap the execution of a Tokio application. It does the following:
///
///  - Start the Tokio I/O pool using a default configuration.
///  - Configure Tokio to make any future spawned with `tokio::spawn` spawn on the pool.
///  - Run the given future to completion on the current thread.
///  - Block the current thread until the pool becomes idle.
///
/// Note that the function will not return immediately once future has completed. Instead it waits
/// for the entire pool to become idle.
///
/// # Examples
///
/// ```no_run
/// #![feature(async_await)]
/// # extern crate tokio_io_pool;
/// # extern crate tokio;
/// # extern crate futures;
/// # use futures_core::{Future, stream::Stream};
/// # use std::marker::Unpin;
/// # fn process<T>(_: T) -> Box<dyn Future<Output = ()> + Send + Unpin> {
/// # unimplemented!();
/// # }
/// # let addr = "127.0.0.1:8080".parse().unwrap();
/// use tokio::net::TcpListener;
///
/// let mut listener = TcpListener::bind(&addr).unwrap();
/// let server = async move {
///     loop {
///         let (socket, _) = listener.accept().await.expect("acccept failed");
///         tokio::spawn(process(socket));
///     }
/// };
///
/// tokio_io_pool::run(server);
/// ```
pub fn run<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut rt = Runtime::new();
    let _ = rt.block_on(future);
    rt.shutdown_on_idle();
}

/// A handle to a [`Runtime`] that allows spawning additional futures from other threads.
#[derive(Clone)]
pub struct Handle {
    workers: Vec<current_thread::Handle>,
    rri: Arc<atomic::AtomicUsize>,
}

impl fmt::Debug for Handle {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Handle")
            .field("nworkers", &self.workers.len())
            .field("next", &self.rri.load(atomic::Ordering::Relaxed))
            .finish()
    }
}

impl tokio::executor::Executor for Handle {
    fn spawn(
        &mut self,
        future: Pin<Box<dyn Future<Output = ()> + 'static + Send>>,
    ) -> Result<(), SpawnError> {
        Handle::spawn(self, future).map(|_| ())
    }
}

impl Handle {
    /// Spawn a future onto a runtime in the pool.
    ///
    /// This spawns the given future onto a single thread runtime's executor. That thread is then
    /// responsible for polling the future until it completes.
    pub fn spawn<F>(&self, future: F) -> Result<&Self, SpawnError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let worker = self.rri.fetch_add(1, atomic::Ordering::Relaxed) % self.workers.len();
        self.workers[worker].spawn(future)?;
        Ok(self)
    }

    /// Spawn all futures yielded by a stream onto the pool.
    ///
    /// This produces a future that accepts futures from a `Stream` and spawns them all onto the
    /// pool round-robin.
    pub fn spawn_all<S>(
        &self,
        stream: S,
    ) -> impl Future<Output = Result<(), StreamSpawnError<<S as TryStream>::Error>>>
    where
        S: TryStream,
        <S as TryStream>::Ok: Future<Output = ()> + Send + 'static,
    {
        Spawner {
            handle: self.clone(),
            stream,
        }
    }
}

/// An I/O-oriented thread pool for executing futures.
///
/// Each thread in the pool has its own I/O reactor, and futures are spawned onto threads
/// round-robin. Futures do not (currently) move between threads in the pool once spawned, and any
/// new futures spawned (using `tokio::spawn`) inside futures are scheduled on the same worker as
/// the original future.
pub struct Runtime {
    handle: Handle,
    threads: Vec<(oneshot::Sender<bool>, thread::JoinHandle<()>)>,
    force_exit: bool,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Runtime")
            .field("nworkers", &self.threads.len())
            .finish()
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// Create a new thread pool with parameters from a default [`Builder`] and return a handle to
    /// it.
    ///
    /// # Panics
    ///
    /// Panics if enough threads could not be spawned (see [`Builder::build`]).
    pub fn new() -> Self {
        Builder::default().build().unwrap()
    }

    /// Return a reference to the pool.
    ///
    /// The returned handle reference can be cloned in order to get an owned value of the handle.
    /// This handle can be used to spawn additional futures onto the pool from other threads.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Spawn a future onto a runtime in the pool.
    ///
    /// This spawns the given future onto a single thread runtime's executor. That thread is then
    /// responsible for polling the future until it completes.
    pub fn spawn<F>(&self, future: F) -> Result<&Self, SpawnError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn(future)?;
        Ok(self)
    }

    /// Spawn all futures yielded by a stream onto the pool.
    ///
    /// This produces a future that accepts futures from a `Stream` and spawns them all onto the
    /// pool round-robin.
    #[must_use]
    pub fn spawn_all<S>(
        &self,
        stream: S,
    ) -> impl Future<Output = Result<(), StreamSpawnError<<S as TryStream>::Error>>>
    where
        S: TryStream,
        <S as TryStream>::Ok: Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn_all(stream)
    }

    /// Run the given future on the pool, and dispatch any child futures spawned with
    /// `tokio::spawn` onto the entire I/O pool.
    ///
    /// Note that child futures of futures that are already running on the pool will be executed on
    /// the same pool thread as their parent. Only the "top-level" calls to `tokio::spawn` are
    /// scheduled to the thread pool as a whole.
    ///
    /// The current thread will be blocked until the given future resolves.
    pub fn block_on<F, O>(&mut self, future: F) -> O
    where
        F: Send + 'static + Future<Output = O>,
        O: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let future = TopLevelSpawn {
            handle: self.handle.clone(),
            future,
        };
        self.handle.spawn(async move {
            let _ = tx.send(future.await);
        }).expect("the runtime is no longer running");
        let mut enter = tokio_executor::enter().expect("already running in executor context");
        enter.block_on(rx).expect("runtime shut down unexpectedly")
    }

    /// Shut down the pool as soon as possible.
    ///
    /// Note that once this method has been called, attempts to spawn additional futures onto the
    /// pool through an outstanding `Handle` may fail. Futures that have not yet resolved will be
    /// dropped.
    ///
    /// The pool will only terminate once any currently-running futures return `NotReady`.
    pub fn shutdown(self) {}

    /// Shut down the pool once all spawned futures have completed.
    ///
    /// Note that once this method has been called, attempts to spawn additional futures onto the
    /// pool through an outstanding `Handle` may fail.
    pub fn shutdown_on_idle(mut self) {
        self.force_exit = false;
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let mut handles = Vec::with_capacity(self.threads.len());
        for (exit, jh) in self.threads.drain(..) {
            if let Err(e) = exit.send(self.force_exit) {
                if !thread::panicking() {
                    panic!("oneshot::Sender::Send: {:?}", e);
                }
            }
            handles.push(jh);
        }
        for jh in handles {
            if let Err(e) = jh.join() {
                if !thread::panicking() {
                    panic!("JoinHandle::join: {:?}", e);
                }
            }
        }
    }
}

/// An error that occurred as a result of spawning futures from a stream given to
/// [`Runtime::spawn_all`].
#[derive(Debug)]
pub enum StreamSpawnError<SE> {
    /// An error occurred while spawning a future yielded by the stream onto the pool.
    Spawn(SpawnError),
    /// An error occurred while polling the stream for another future.
    Stream(SE),
}

impl<SE> From<SE> for StreamSpawnError<SE> {
    fn from(e: SE) -> Self {
        StreamSpawnError::Stream(e)
    }
}

struct Spawner<S> {
    handle: Handle,
    stream: S,
}

impl<S: Unpin> Unpin for Spawner<S> {}

impl<S> fmt::Debug for Spawner<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Spawner")
            .field("stream", &self.stream)
            .finish()
    }
}

impl<S> Future for Spawner<S>
where
    S: TryStream,
    <S as TryStream>::Ok: Future<Output = ()> + Send + 'static,
{
    type Output = Result<(), StreamSpawnError<<S as TryStream>::Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        // our type is structural, so field projection through Pin is safe:
        // https://doc.rust-lang.org/std/pin/index.html#projections-and-structural-pinning
        while let Some(fut) =
            ready!(unsafe { self.as_mut().map_unchecked_mut(|s| &mut s.stream) }.try_poll_next(cx))
        {
            self.handle.spawn(fut?).map_err(StreamSpawnError::Spawn)?;
        }
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
struct TopLevelSpawn<F>{
    handle: Handle,
    future: F
}

impl<F: Unpin> Unpin for TopLevelSpawn<F> {}

impl<F> Future for TopLevelSpawn<F> where F: Future {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        // our type is structural, so field projection through Pin is safe:
        // https://doc.rust-lang.org/std/pin/index.html#projections-and-structural-pinning
        let (handle, pin) = unsafe {
            let this = self.get_unchecked_mut();
            (&mut this.handle, Pin::new_unchecked(&mut this.future))
        };
        tokio_executor::with_default(handle, move || pin.poll(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    use futures::future;
    use futures_util::future::FutureExt;
    use futures_util::stream::StreamExt;
    use std::io::{Read, Write};
    use tokio::io::AsyncReadExt;

    #[test]
    fn it_works() {
        let (tx, rx) = oneshot::channel();

        let mut rt = Runtime::new();
        rt.spawn(async move {
            tx.send(()).unwrap();
        })
        .unwrap();
        assert_eq!(rt.block_on(rx).unwrap(), ());
        rt.shutdown_on_idle();
    }

    #[test]
    fn spawn_all() {
        let addr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(&addr).expect("unable to bind TCP listener");
        let addr = listener.local_addr().unwrap();

        let rt = Builder::default().pool_size(1).build().unwrap();

        // spawn all connections onto the pool
        let spawner = rt.spawn_all(
            listener
            .incoming()
            .then(|r| async { r.unwrap() })
            .map(|sock| -> Result<_, ()> {
                Ok(async {
                    let (mut reader, mut writer) = sock.split();
                    let bytes_copied = reader.copy(&mut writer);
                    let _ = bytes_copied.await.unwrap();
                })
            }));

        // spawn the spawner onto the pool too
        // (a "real" server might wait for it instead)
        rt.spawn(spawner.map(|r| r.unwrap()))
            .unwrap();

        let mut client = ::std::net::TcpStream::connect(&addr).unwrap();
        client.write_all(b"hello world").unwrap();
        client.shutdown(::std::net::Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes, b"hello world");

        let mut client = ::std::net::TcpStream::connect(&addr).unwrap();
        client.write_all(b"bye world").unwrap();
        client.shutdown(::std::net::Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes, b"bye world");
    }

    #[test]
    fn run() {
        let addr = "127.0.0.1:0".parse().unwrap();
        let mut listener = tokio::net::TcpListener::bind(&addr).expect("unable to bind TCP listener");
        let addr = listener.local_addr().unwrap();

        thread::spawn(move || {
            super::run(async move {
                loop {
                    let (sock, _) = listener.accept().await.unwrap();
                    tokio::spawn(async move {
                        let (mut reader, mut writer) = sock.split();
                        let bytes_copied = reader.copy(&mut writer);
                        let _ = bytes_copied.await.unwrap();
                    });
                }
            });
        });

        let mut client = ::std::net::TcpStream::connect(&addr).unwrap();
        client.write_all(b"hello world").unwrap();
        client.shutdown(::std::net::Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes, b"hello world");

        let mut client = ::std::net::TcpStream::connect(&addr).unwrap();
        client.write_all(b"bye world").unwrap();
        client.shutdown(::std::net::Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes, b"bye world");
    }

    // futures channels can exchange information between different threads
    #[test]
    fn interthread_communication() {
        let (tx0, rx0) = oneshot::channel::<u32>();
        let (tx1, rx1) = oneshot::channel::<u32>();

        let mut rt = Builder::default().pool_size(2).build().unwrap();
        rt.spawn(async move {
            tx0.send(42).unwrap()
        })
            .unwrap();
        rt.spawn(async move {
            let v = rx0.await.unwrap();
            tx1.send(v + 1).unwrap();
        })
            .unwrap();
        assert_eq!(rt.block_on(rx1).unwrap(), 43);
        rt.shutdown_on_idle();
    }

    // A Future that isn't Send can't be spawned into the Runtime, but it _can_ be spawned from a
    // thread onto that same thread
    #[test]
    fn spawn_nonsend_futures() {
        use std::rc::Rc;

        let rt = Runtime::new();
        // NOTE: we need to use lazy, not async {} here, becuase async {} wouldn't be Send if we
        // construct an Rc inside of it.
        rt.spawn(future::lazy(move |_| {
                let (tx, rx) = oneshot::channel::<u32>();
                let x = Rc::new(42u32); // Note: Rc is not Send
                tokio_current_thread::spawn(async move {
                    tx.send(*x).unwrap();
                });
                rx
            }).then(|rx| {
                async move {
                    let v = rx.await.unwrap();
                    assert_eq!(42, v);
                }
            })).unwrap();
        rt.shutdown_on_idle();
    }

    #[test]
    fn really_lazy() {
        super::run(async {
            tokio::spawn(async { () });
        });
    }
}
