# tokio-io-pool

[![Crates.io](https://img.shields.io/crates/v/tokio-io-pool.svg)](https://crates.io/crates/tokio-io-pool)
[![Documentation](https://docs.rs/tokio-io-pool/badge.svg)](https://docs.rs/tokio-io-pool/)
[![Build Status](https://travis-ci.org/jonhoo/tokio-io-pool.svg?branch=master)](https://travis-ci.org/jonhoo/tokio-io-pool)

This crate provides a thread pool for executing I/O-heavy futures.

The standard `Runtime` provided by `tokio` uses a thread-pool to allow concurrent execution of
compute-heavy futures. However, it (currently) uses only a single I/O reactor to drive all
network and file activity. While this trade-off works well for many asynchronous applications,
it is not a great fit for high-performance I/O bound applications that are bottlenecked
primarily by system calls.

This crate provides an alternative implementation of a futures-based thread pool. It spawns a
pool of threads that each runs a `tokio::runtime::current_thread::Runtime` (and thus each have
an I/O reactor of their own), and spawns futures onto the pool by assigning the future to
threads round-robin. Once a future has been spawned onto a thread, it, and any child futures it
may produce through `tokio::spawn`, remain under the control of that same thread.
