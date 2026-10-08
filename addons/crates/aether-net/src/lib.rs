//! The HTTP base every aether downloader shares: one connection pool, a
//! connection [`Limiter`] (global and per host), a [`RetryPolicy`], an
//! [`Events`] channel, plain and bulk ranged downloads, and the one error
//! type, [`SourceError`], that the downloaders built on it return.
//!
//! Everything network-facing is async (tokio + reqwest). Whole-file
//! downloads write into a caller-supplied [`BlobSink`]; sink calls may
//! block and are always made from `spawn_blocking`, never on a tokio
//! worker.
//!
//! The modules are public (and hidden from the docs) only so the
//! downloader crates can reach the helpers they share (`http::check`,
//! `events::Job`, `error::redact`, …); the stable API is the re-exports
//! below.

#[doc(hidden)]
pub mod bulk;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod events;
#[doc(hidden)]
pub mod http;
#[doc(hidden)]
pub mod http_file;
#[doc(hidden)]
pub mod limit;
#[doc(hidden)]
pub mod retry;
#[doc(hidden)]
pub mod sink;

pub use bulk::{BulkHttp, BulkHttpConfig, RangeBody};
pub use error::{Result, SourceError};
pub use events::{Events, JobId, SourceEvent};
pub use http::{Http, HttpConfig};
pub use http_file::{Downloaded, HttpFile};
pub use limit::{Limiter, Permit};
pub use retry::RetryPolicy;
pub use sink::{BlobSink, MemorySink};
