//! Compile-time check: every public async entry point's future is `Send`,
//! so callers can `tokio::spawn` them on a multi-threaded runtime.
#![allow(dead_code, clippy::diverging_sub_expression)]

use std::sync::Arc;

use aether_net::BlobSink;
use aether_wj_cdn::CdnFile;

fn assert_send<T: Send>(_: T) {}

/// Never called: it only has to type-check.
fn futures_are_send(cdn: CdnFile, sink: Arc<dyn BlobSink>) {
    assert_send(async move { cdn.download(sink, None).await });
}

#[test]
fn compiles() {}
