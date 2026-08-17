//! **The real conformance suite, run against a JavaScript-authored provider.**
//!
//! Spec §10's requirement is one suite, not one per language:
//!
//! > One conformance suite, run against every provider in every language. […]
//! > Bindings expose `assert_conformance` so a host-language provider is held to
//! > exactly the same standard as a Rust one.
//!
//! So this module contains no test cases. It calls
//! [`vfs_embed::assert_conformance`] — the same function `DiskProvider`,
//! `ZipProvider`, `MemoryProvider`, `LayeredProvider`, `RouterProvider`,
//! `ReadOnlyProvider` and `SeekableProvider` are held to in their own test
//! suites — on the `Arc<dyn Provider>` behind a handle. A JS provider reaches it
//! through task 7's `NodeProvider` bridge with nothing about the suite adapted
//! for it. A second suite written in TypeScript would drift from the first, and
//! then the two would disagree about what a provider owes, which is worse than
//! having only one.
//!
//! ## Why it is a `Promise` and not a blocking call
//!
//! [`assert_conformance`] issues on the order of a hundred blocking provider
//! calls. For a JS-authored provider each one has to be serviced by that
//! provider's event loop, and task 7's deadlock guard refuses any call issued
//! *from* that loop — so a synchronous `assertConformance()` would work only for
//! a provider registered in a worker, and would refuse (correctly, but
//! unhelpfully) for the simplest thing a host writes: a provider object declared
//! in the same file as the test.
//!
//! Running the suite as an `AsyncTask` puts it on a libuv pool thread, which is
//! never any JS loop. `await assertConformance(p)` then works for a provider on
//! the main loop *and* one in a worker, because awaiting is exactly what leaves
//! the servicing loop free to run the callbacks. The one thing a host must not do
//! is block its loop while awaiting; that is the same rule as everywhere else in
//! this binding, and it is the natural way to write JavaScript anyway.
//!
//! ## Why the panic is caught
//!
//! `assert_conformance` reports by panicking with a message naming the failing
//! case — the right design for a Rust test harness and unusable across an FFI
//! boundary, where an unwind out of an `extern "C"` frame aborts the process.
//! `rust/Cargo.toml` sets `panic = "unwind"` for both profiles with the comment
//! *"everything here unwinds so the library can catch panics at FFI boundaries
//! instead of killing its host process"*, which is precisely this. So the run is
//! wrapped in [`std::panic::catch_unwind`] and the payload becomes the rejected
//! promise's message. The default panic hook still prints the file and line to
//! stderr, which is where the location comes from.

use std::panic::AssertUnwindSafe;
use std::time::Instant;

use napi::bindgen_prelude::{AsyncTask, Buffer};
use napi::{Env, Error, Result, Task};
use napi_derive::napi;

use vfs_embed::{Access, Provider as VfsProvider, FIXTURE_FILES};

use crate::{lookup_provider, primitives::access_name};

/// What a passing conformance run reports.
///
/// It is deliberately more than `true`. A conformance runner that passes
/// everything is indistinguishable from no runner at all, so the report carries
/// the two facts that make a pass checkable: **which case groups ran** (derived
/// from the declared access level, the same way the suite itself dispatches) and
/// **how many provider calls the suite made** — a number a host can assert is in
/// the dozens rather than zero.
#[napi(object)]
pub struct ConformanceReport {
    pub handle: u32,
    /// The kind of provider, as `provider.kind` reports it.
    pub kind: Option<String>,
    /// `'seqread'`, `'read'` or `'readwrite'` — what the provider declared, and
    /// therefore which cases it was held to.
    pub access: String,
    pub immutable: bool,
    pub slow: bool,
    pub preferred_block: Option<u32>,
    /// The case groups that ran: `'common'`, then `'sequential'` or
    /// `'positional'`, plus `'writable'` for a `readwrite` provider.
    pub cases: Vec<String>,
    /// Provider calls that crossed the bridge during the run. **This is the
    /// number that says the suite did work**: a JS provider that passed with
    /// `providerCalls: 0` was not tested, it was skipped.
    ///
    /// For a Rust provider, which has no bridge, the key is **absent** — so JS
    /// reads it as `undefined`, not `null`. This is a `#[napi(object)]` *field*,
    /// and napi-derive omits a `None` field rather than setting it to null; only
    /// an `Option<T>` *return* (`Provider::stats`, `Provider::cache_stats`,
    /// `Session::getattr`) arrives as `null`. `providerCalls === null` is a check
    /// that never matches, and this comment said `null` until task 2 measured it.
    pub provider_calls: Option<f64>,
    pub duration_ms: f64,
}

/// The case groups [`vfs_embed::assert_conformance`] runs for a given access
/// level. Mirrors its own `match`, and is the reason the report can say what was
/// checked rather than only that nothing failed.
fn case_groups(access: Access) -> Vec<String> {
    let mut out = vec!["common".to_string()];
    out.push(
        match access {
            Access::SeqRead => "sequential",
            Access::Read | Access::ReadWrite => "positional",
        }
        .to_string(),
    );
    if access == Access::ReadWrite {
        out.push("writable".to_string());
    }
    out
}

/// Turn a `catch_unwind` payload into something a JS developer can act on.
///
/// `assert_conformance`'s messages already name the case (*"read_at on a SeqRead
/// provider succeeded with 4 bytes; it must be refused"*), so the payload is the
/// valuable part; the file and line went to stderr through the default hook.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    "the conformance suite panicked with a non-string payload".to_string()
}

pub struct ConformanceTask {
    handle: u32,
}

impl Task for ConformanceTask {
    type Output = ConformanceReport;
    type JsValue = ConformanceReport;

    /// Runs on a libuv pool thread — see the module docs on why that is what
    /// makes this work for a provider on any loop.
    fn compute(&mut self) -> Result<Self::Output> {
        let p: std::sync::Arc<dyn VfsProvider> = lookup_provider(self.handle)?;
        let caps = p.capabilities();
        let before = crate::jsprovider::stats_for(self.handle).map(|s| s.calls);
        let started = Instant::now();

        // The whole suite, unmodified. `AssertUnwindSafe` because `Arc<dyn
        // Provider>` is not `UnwindSafe` (it has interior mutability, as every
        // provider with a handle table does) and a panic here does not leave a
        // provider a later call will observe — the promise rejects and the host
        // is expected to throw the provider away.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            vfs_embed::assert_conformance(p);
        }));
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;

        if let Err(payload) = outcome {
            let after = crate::jsprovider::stats_for(self.handle).map(|s| s.calls);
            let calls = match (before, after) {
                (Some(b), Some(a)) => format!(" after {} provider calls", (a - b) as u64),
                _ => String::new(),
            };
            return Err(Error::from_reason(format!(
                "assertConformance(provider {}) failed{calls}: {}. This is the same \
                 suite every Rust provider is held to — see \
                 vfs-provider/src/conformance.rs. The panic's file and line are on \
                 stderr.",
                self.handle,
                panic_message(payload),
            )));
        }

        let after = crate::jsprovider::stats_for(self.handle).map(|s| s.calls);
        Ok(ConformanceReport {
            handle: self.handle,
            kind: crate::primitives::kind_of(self.handle).map(|k| k.to_string()),
            access: access_name(caps.access).to_string(),
            immutable: caps.immutable,
            slow: caps.slow,
            preferred_block: caps.preferred_block,
            cases: case_groups(caps.access),
            provider_calls: match (before, after) {
                (Some(b), Some(a)) => Some(a - b),
                _ => None,
            },
            duration_ms,
        })
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Run the workspace's conformance suite against a provider.
///
/// **This is stage 4's gate:** a host-authored provider held to the identical
/// contract a Rust one is. The suite is
/// [`vfs_embed::assert_conformance`] with nothing adapted — the cases it runs are
/// selected by the provider's *declared* capabilities, so a `seqread` provider
/// faces the sequential cases (including "a positional read must be refused")
/// and a `readwrite` one faces the write cases as well.
///
/// The provider must expose the reference tree, which
/// [`conformance_fixture`] hands over so a host holds no second copy of it.
///
/// Resolves to a [`ConformanceReport`]; **rejects** with the failing case's
/// message. Composed providers work too, which is how spec §10's own example —
/// `assert_conformance(seekable(seq_fixture()))` — is written from a host.
#[napi(js_name = "assertConformance", ts_return_type = "Promise<ConformanceReport>", catch_unwind)]
pub fn assert_conformance(provider: u32) -> AsyncTask<ConformanceTask> {
    AsyncTask::new(ConformanceTask { handle: provider })
}

/// One file of the reference tree a conformance-tested provider must serve.
#[napi(object)]
pub struct FixtureFile {
    pub path: String,
    pub bytes: Buffer,
}

/// The reference tree, from the same constant Rust reads.
///
/// A host-language provider has to serve exactly this to pass conformance, and
/// hard-coding it in JavaScript would put a second copy of the contract in a
/// place that cannot be kept in step with the first. `FIXTURE_FILES` grows a
/// file one day and a hard-coded provider fails with "readdir of the root listed
/// …" and no clue why.
#[napi(js_name = "conformanceFixture", catch_unwind)]
pub fn conformance_fixture() -> Vec<FixtureFile> {
    FIXTURE_FILES
        .iter()
        .map(|(path, bytes)| FixtureFile {
            path: (*path).to_string(),
            bytes: Buffer::from(*bytes),
        })
        .collect()
}

/// Write the reference tree into a real directory, for testing a disk-backed
/// provider — `assertConformance(disk(dir))`.
///
/// **Clears `dir` first**, which is [`vfs_embed::write_fixture_tree`]'s
/// behaviour and worth repeating here: a leftover file from a previous run would
/// show up in `readdir` of the root and fail the suite for a reason that has
/// nothing to do with the provider.
#[napi(js_name = "writeConformanceFixture", catch_unwind)]
pub fn write_conformance_fixture(dir: String) -> Result<()> {
    let p = std::path::PathBuf::from(&dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::from_reason(format!("writeConformanceFixture({dir:?}): {e}")))?;
    }
    // `write_fixture_tree` panics on failure (it is a test helper), so the panic
    // is caught here for the same reason the suite's is: a host gets an error,
    // not a dead process.
    std::panic::catch_unwind(|| vfs_embed::write_fixture_tree(&p)).map_err(|payload| {
        Error::from_reason(format!(
            "writeConformanceFixture({dir:?}) failed: {}",
            panic_message(payload)
        ))
    })
}
