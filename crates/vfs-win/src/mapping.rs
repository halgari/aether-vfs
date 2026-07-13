//! File-mapping-backed shared memory. All Win32 FFI is confined here.

/// RAII owner of a Windows file-mapping section and its mapped view.
pub struct SharedMapping;
