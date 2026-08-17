//! **Structural: every `#[napi]` entry point must carry `catch_unwind`.**
//!
//! A Rust panic that unwinds out of an `extern "C"` frame is an immediate
//! `abort()`. Task 1 paid this debt on the shim side — all twenty `extern
//! "system"` ntdll hooks route through one containment wrapper — and the
//! workspace adopted `panic = "unwind"` in both profiles specifically so a
//! library could catch panics at its FFI boundaries instead of killing its host.
//!
//! **napi-derive does not do that by default.** It emits
//! `std::panic::catch_unwind` around a generated entry point *only* when the
//! attribute says `catch_unwind`
//! (`napi-derive-backend-1.0.75/src/codegen/fn.rs`, where `function_call` is
//! wrapped `if self.catch_unwind`; `napi-derive-2.16.13/src/parser/mod.rs`
//! refuses the flag anywhere but a function or method, so it cannot be set once
//! for an `impl` block). Without it, a panic anywhere under a `#[napi]` function
//! takes down the Node process — not the call, the process, with every other
//! session, worker and provider in it.
//!
//! "We currently never panic" is not containment: this crate holds no `unwrap`
//! or `expect` on any reachable path, and that is a property of today's code,
//! re-established by hand at every edit. This test is the property that survives
//! an edit.
//!
//! ## Why it reads the source text
//!
//! There is no runtime handle on "which entry points were generated with
//! containment" — the flag is consumed by a proc macro and leaves nothing
//! behind to query. So the check is a text check, exactly as
//! `vfs-shim`'s `no_extern_hook_bypasses_the_panic_containment_macro` and
//! `vfs-directord`'s `daemon_names_only_the_embed_api` are, and for the same
//! reason: the alternative is a guard that cannot see the thing it guards.
//!
//! ## Why the file list is derived
//!
//! Because the shim's first version of this idea named its files
//! (`include_str!("hook.rs")`) and therefore missed `lazy_section.rs`, which held
//! an uncontained entry point for the whole of stage 4. The enumeration below is
//! read off `src/` at test time, so a new module cannot be added outside the
//! check.
//!
//! ## Why this lives in `tests/`
//!
//! `[lib] test = false` (see `Cargo.toml`) keeps a unit-test harness that cannot
//! resolve a single `napi_*` symbol out of `cargo test --workspace`. An
//! integration test is a separate binary that does not link the cdylib at all —
//! it only reads files — so it runs clean under the ordinary workspace test
//! command, which is the whole point of having it.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `src/`, recursively. Derived, never listed.
fn sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&src, &mut out);
    out.sort();
    out
}

/// One `#[napi(...)]` attribute and what it is attached to.
struct Attr {
    file: String,
    line: usize,
    text: String,
    /// The first line of the item the attribute decorates, past any further
    /// attributes and doc comments.
    item: String,
}

fn attrs() -> Vec<Attr> {
    let mut out = Vec::new();
    for path in sources() {
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let lines: Vec<&str> = src.lines().collect();
        for (i, raw) in lines.iter().enumerate() {
            let text = raw.trim();
            // `#[napi` in a doc comment or a string is prose, not an attribute.
            if !text.starts_with("#[napi") {
                continue;
            }
            assert!(
                text.ends_with(']'),
                "{name}:{} — a multi-line `#[napi(...)]` attribute; this check reads one \
                 line per attribute. Put it on one line.",
                i + 1
            );
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.starts_with("#[") || t.starts_with("//") {
                    j += 1;
                } else {
                    break;
                }
            }
            out.push(Attr {
                file: name.clone(),
                line: i + 1,
                text: text.to_string(),
                item: lines.get(j).unwrap_or(&"").trim().to_string(),
            });
        }
    }
    out
}

/// Whether the item this attribute decorates is a function or a method — the
/// only two things napi-derive accepts `catch_unwind` on, and the only two that
/// generate an `extern "C"` entry point with a body that can panic.
fn decorates_a_function(item: &str) -> bool {
    item.starts_with("fn ")
        || item.starts_with("pub fn ")
        || item.starts_with("pub(crate) fn ")
        || item.starts_with("async fn ")
        || item.starts_with("pub async fn ")
        || item.starts_with("unsafe fn ")
        || item.starts_with("pub unsafe fn ")
}

#[test]
fn every_napi_function_carries_catch_unwind() {
    let all = attrs();
    // A broken enumeration must fail loudly rather than pass vacuously. Every
    // module of the crate contributes entry points; the count is a floor, not a
    // target.
    let functions: Vec<&Attr> = all.iter().filter(|a| decorates_a_function(&a.item)).collect();
    assert!(
        functions.len() >= 40,
        "the enumeration found only {} `#[napi]` functions across {} attributes — it is \
         not reading the crate's sources",
        functions.len(),
        all.len()
    );

    let missing: Vec<String> = functions
        .iter()
        .filter(|a| !a.text.contains("catch_unwind"))
        .map(|a| format!("  {}:{} {}  on  {}", a.file, a.line, a.text, a.item))
        .collect();

    assert!(
        missing.is_empty(),
        "{} `#[napi]` function(s) do not carry `catch_unwind`, so a Rust panic in one \
         aborts the whole Node process instead of throwing:\n{}\n\nAdd `catch_unwind` to \
         the attribute — `#[napi(catch_unwind)]`, `#[napi(getter, catch_unwind)]`, and so \
         on. napi-derive emits the `catch_unwind` only when asked, and refuses the flag on \
         an `impl` block, so it has to be per function. See this file's module docs.",
        missing.len(),
        missing.join("\n")
    );
}

/// The canary has to exist for `panic_surfaces_as_a_js_exception` in
/// `test/panic.test.cjs` to have anything to call, and that JS test is the only
/// place the containment is demonstrated rather than asserted structurally. A
/// silently deleted canary would leave the JS test skipping.
#[test]
fn the_panic_canary_is_still_exported_and_contained() {
    let canary: Vec<Attr> = attrs()
        .into_iter()
        .filter(|a| a.item.contains("fn panic_for_test"))
        .collect();
    assert_eq!(
        canary.len(),
        1,
        "`panicForTest` is the only reachable panic in this crate and the only thing \
         `test/panic.test.cjs` can use to prove a panic becomes a JS exception. Removing it \
         makes that test unable to fail."
    );
    assert!(
        canary[0].text.contains("catch_unwind"),
        "the panic canary itself is uncontained — the JS test would abort the process"
    );
}
