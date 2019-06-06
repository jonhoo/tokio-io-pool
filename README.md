# tokio-io-pool

[![Crates.io](https://img.shields.io/crates/v/tokio-io-pool.svg)](https://crates.io/crates/tokio-io-pool)
[![Documentation](https://docs.rs/tokio-io-pool/badge.svg)](https://docs.rs/tokio-io-pool/)
[![Build Status](https://travis-ci.com/jonhoo/tokio-io-pool.svg?branch=master)](https://travis-ci.com/jonhoo/tokio-io-pool)
[![Codecov](https://codecov.io/github/jonhoo/tokio-io-pool/coverage.svg?branch=master)](https://codecov.io/gh/jonhoo/tokio-io-pool)

This crate provides a thread pool for executing short, I/O-heavy futures efficiently.

The standard `Runtime` provided by `tokio` uses a thread-pool to allow concurrent execution of
compute-heavy futures. However, its work-stealing makes it so that futures may be executed on
different threads to where their reactor are running, which results in unnecessary
synchronization, and thus lowers the achievable throughput. While this trade-off works well for
many asynchronous applications, since it spreads load more evenly, it is not a great fit for
high-performance I/O bound applications where the cost of synchronizing threads is high. This
can happen, for example, if your application performs frequent but small I/O operations.

This crate provides an alternative implementation of a futures-based thread pool. It spawns a
pool of threads that each runs a `tokio::runtime::current_thread::Runtime` (and thus each have
an I/O reactor of their own), and spawns futures onto the pool by assigning the future to
threads round-robin. Once a future has been spawned onto a thread, it, and any child futures it
may produce through `tokio::spawn`, remain under the control of that same thread.

In general, you should use `tokio-io-pool` only if you perform a lot of very short I/O
operations on many futures, and find that you are bottlenecked by work-stealing or reactor
notifications with the regular `tokio` runtime. If you are unsure what to use, start with the
`tokio` runtime.

Be aware that this pool does *not* support the
[`blocking`](https://docs.rs/tokio-threadpool/0.1.5/tokio_threadpool/fn.blocking.html) function
since it is [not supported](https://github.com/tokio-rs/tokio/issues/432) by the underlying
`current_thread::Runtime`. Hopefully this will be rectified down the line.

There is some discussion around trying to merge this pool into `tokio` proper; that effort is
tracked in [tokio-rs/tokio#486](https://github.com/tokio-rs/tokio/issues/486).

## Examples

```rust
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
