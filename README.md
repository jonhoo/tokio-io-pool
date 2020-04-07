# tokio-io-pool

[![Crates.io](https://img.shields.io/crates/v/tokio-io-pool.svg)](https://crates.io/crates/tokio-io-pool)

This crate provided a thread pool for executing short, I/O-heavy futures efficiently.

It is no longer necessary following the [scheduler
improvements](https://tokio.rs/blog/2019-10-scheduler/) in `tokio
0.2.0-alpha.7`, which handle concurrent I/O on many sockets much better
than the old scheduler did. Tokio 0.2 also does not allow the same hooks
to provide a custom scheduler, so even if it were necessary, updating
`tokio-io-pool` to work as "transparently" as it did with `tokio 0.1`
would be difficult. For that reason, this project has been discontinued.
If you find an issue with tokio's performance, file it as a tokio bug :)
