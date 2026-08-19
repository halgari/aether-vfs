// A module that fails at module scope rather than exporting anything —
// `import()`'s promise rejects before `provider-host.mts` ever gets to look at
// exports. This is the case the release handler being registered *before* the
// `await import(...)` exists for: without it, a release arriving while this
// throw is in flight would have nowhere to land.

throw new Error('boom');
