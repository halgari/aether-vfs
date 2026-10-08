//! Compile-time check: every public async entry point's future is `Send`,
//! so callers can `tokio::spawn` them on a multi-threaded runtime.
#![allow(dead_code, clippy::diverging_sub_expression)]

use std::sync::Arc;

use aether_nexus::{NexusArchive, NexusClient};

fn assert_send<T: Send>(_: T) {}

/// Never called: it only has to type-check.
fn futures_are_send(client: Arc<NexusClient>, archive: Arc<NexusArchive>) {
    let c = client.clone();
    assert_send(async move { NexusArchive::open(c, 1).await });
    let a = archive.clone();
    assert_send(async move { a.read_range(0, 0..1).await });
    let a = archive.clone();
    assert_send(async move { a.read_entry(0).await });
    let c = client.clone();
    assert_send(async move { c.download_url(1).await });
    assert_send(async move { client.file_record("skyrimspecialedition", 1).await });
}

#[test]
fn compiles() {}
