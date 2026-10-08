//! Compile-time check: every public async entry point's future is `Send`,
//! so callers can `tokio::spawn` them on a multi-threaded runtime.
#![allow(dead_code, clippy::diverging_sub_expression)]

use std::sync::Arc;

use aether_net::{BlobSink, HttpFile};

fn assert_send<T: Send>(_: T) {}

/// Never called: it only has to type-check.
fn futures_are_send(file: HttpFile, sink: Arc<dyn BlobSink>) {
    assert_send(async move { file.download(sink, None, None).await });
}

#[test]
fn compiles() {}
