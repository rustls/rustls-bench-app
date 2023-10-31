# Rustls bench runner application

This crate implements an HTTP server that runs rustls benchmarks and reports results based on GitHub
activity. It uses `axum` to process HTTP requests and SQLite to persist data (through `sqlx`).

To minimize noise in timing benchmarks, the server enqueues incoming GitHub events and handles them
sequentially. The goal is to do as little work as possible because benchmarks may be running. The
same principle is applied to other areas of the server, like serving cachegrind diffs: we cache the
diffs after benchmarking, so they can be served from the database instead (this approach is less
flexible than calculating the diff on-the-go, but it fits our use case better).
