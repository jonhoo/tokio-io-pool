//! This crate provides a thread pool for executing I/O-heavy futures.
//!
//! The standard `Runtime` provided by `tokio` uses a thread-pool to allow concurrent execution of
//! compute-heavy futures. However, it (currently) uses only a single I/O reactor to drive all
//! network and file activity. While this trade-off works well for many asynchronous applications,
//! it is not a great fit for high-performance I/O bound applications that are bottlenecked
//! primarily by system calls.
//!
//! This crate provides an alternative implementation of a futures-based thread pool. It spawns a
//! pool of threads that each runs a `tokio::runtime::current_thread::Runtime` (and thus each have
//! an I/O reactor of their own), and spawns futures onto the pool by assigning the future to
//! threads round-robin. Once a future has been spawned onto a thread, it, and any child futures it
//! may produce through `tokio::spawn`, remain under the control of that same thread.

#![deny(missing_docs)]
#![deny(unused_extern_crates)]

#[macro_use]
extern crate futures;
extern crate num_cpus;
extern crate tokio;

use futures::sync::oneshot;
use std::sync::{atomic, mpsc, Arc};
use std::{io, thread};
use tokio::executor::SpawnError;
use tokio::prelude::*;
use tokio::runtime::current_thread;

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
    after_start: Option<Arc<Fn() + Send + Sync>>,
    before_stop: Option<Arc<Fn() + Send + Sync>>,
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
            handle: handle,
        })
    }
}

/// A handle to a [`Runtime`] that allows spawning additional futures from other threads.
#[derive(Clone)]
pub struct Handle {
    workers: Vec<current_thread::Handle>,
    rri: Arc<atomic::AtomicUsize>,
}

impl Handle {
    /// Spawn a future onto a runtime in the pool.
    ///
    /// This spawns the given future onto a single thread runtime's executor. That thread is then
    /// responsible for polling the future until it completes.
    pub fn spawn<F>(&self, future: F) -> Result<&Self, SpawnError>
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let worker = self.rri.fetch_add(1, atomic::Ordering::SeqCst) % self.workers.len();
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
    ) -> impl Future<Item = (), Error = StreamSpawnError<<S as Stream>::Error>>
    where
        S: Stream,
        <S as Stream>::Item: Future<Item = (), Error = ()> + Send + 'static,
    {
        Spawner {
            handle: self.clone(),
            stream,
        }
    }
}

/// An I/O-oriented thread pool for executing futures.
///
/// Each thread in the pool has its own I/O reactor, and futures are spawned onto futures
/// round-robin. Futures do not (currently) move between threads in the pool once spawned, and any
/// new futures spawned (using `tokio::spawn`) inside futures are scheduled on the same worker as
/// the original future.
pub struct Runtime {
    handle: Handle,
    threads: Vec<(oneshot::Sender<bool>, thread::JoinHandle<()>)>,
    force_exit: bool,
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

    /// Return an additional reference to the pool.
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
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        self.handle.spawn(future)?;
        Ok(self)
    }

    /// Spawn all futures yielded by a stream onto the pool.
    ///
    /// This produces a future that accepts futures from a `Stream` and spawns them all onto the
    /// pool round-robin.
    pub fn spawn_all<S>(
        &self,
        stream: S,
    ) -> impl Future<Item = (), Error = StreamSpawnError<<S as Stream>::Error>>
    where
        S: Stream,
        <S as Stream>::Item: Future<Item = (), Error = ()> + Send + 'static,
    {
        self.handle.spawn_all(stream)
    }

    /// Shutdown the pool once all spawned futures have completed.
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
            exit.send(self.force_exit).unwrap();
            handles.push(jh);
        }
        for jh in handles {
            jh.join().unwrap();
        }
    }
}

/// An error that occurred as a result of spawning futures from a stream given to
/// [`Runtime::spawn_all`].
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

impl<S> Future for Spawner<S>
where
    S: Stream,
    <S as Stream>::Item: Future<Item = (), Error = ()> + Send + 'static,
{
    type Item = ();
    type Error = StreamSpawnError<<S as Stream>::Error>;

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        while let Some(fut) = try_ready!(self.stream.poll()) {
            self.handle.spawn(fut).map_err(StreamSpawnError::Spawn)?;
        }
        Ok(Async::Ready(()))
    }
}
