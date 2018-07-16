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

Be aware that this pool does *not* support the
[`blocking`](https://docs.rs/tokio-threadpool/0.1.5/tokio_threadpool/fn.blocking.html) function
since it is [not supported](https://github.com/tokio-rs/tokio/issues/432) by the underlying
`current_thread::Runtime`. Hopefully this will be rectified down the line.

There is some discussion around trying to merge this pool into `tokio` proper; that effort is
tracked in [tokio-rs/tokio#486](https://github.com/tokio-rs/tokio/issues/486).

## Examples

```rust
extern crate tokio_io_pool;
extern crate tokio;

use tokio::prelude::*;
use tokio::io::copy;
use tokio::net::TcpListener;

fn main() {
    // Bind the server's socket.
    let addr = "127.0.0.1:12345".parse().unwrap();
    let listener = TcpListener::bind(&addr)
        .expect("unable to bind TCP listener");

    // Pull out a stream of sockets for incoming connections
    let server = listener.incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(|sock| {
            // Split up the reading and writing parts of the
            // socket.
            let (reader, writer) = sock.split();

            // A future that echos the data and returns how
            // many bytes were copied...
            let bytes_copied = copy(reader, writer);

            // ... after which we'll print what happened.
            let handle_conn = bytes_copied.map(|amt| {
                println!("wrote {:?} bytes", amt)
            }).map_err(|err| {
                eprintln!("IO error {:?}", err)
            });

            // Spawn the future as a concurrent task.
            tokio::spawn(handle_conn)
        });

    // Start the Tokio runtime
    tokio_io_pool::run(server);
}
```
