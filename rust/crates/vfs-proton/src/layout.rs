use std::io;
use std::path::{Path, PathBuf};

/// The base directory for everything this crate writes: downloaded tarballs
/// and extracted runtimes both live under here, never in `umu`'s default
/// location or any other system path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    base: PathBuf,
}

/// A tag failed [`Root::try_runtime_dir`]'s validation: it was empty,
/// absolute, or tried to leave `runtimes()` via a path separator or `..`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTag(pub String);

impl std::fmt::Display for InvalidTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid runtime tag: {:?}", self.0)
    }
}

impl std::error::Error for InvalidTag {}

impl Root {
    /// Resolves the base directory from the environment:
    /// `$AETHER_VFS_HOME` if set, else `$XDG_DATA_HOME/aether-vfs`, else
    /// `$HOME/.local/share/aether-vfs`. On Windows, where `$HOME` is
    /// typically unset, falls back to `%LOCALAPPDATA%\aether-vfs` so the
    /// crate is usable for tests and development there.
    pub fn from_env() -> io::Result<Root> {
        if let Ok(home) = std::env::var("AETHER_VFS_HOME") {
            return Ok(Root::at(PathBuf::from(home)));
        }
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            return Ok(Root::at(PathBuf::from(xdg).join("aether-vfs")));
        }
        if let Ok(home) = std::env::var("HOME") {
            return Ok(Root::at(PathBuf::from(home).join(".local/share/aether-vfs")));
        }
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            return Ok(Root::at(PathBuf::from(local_app_data).join("aether-vfs")));
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not resolve a home directory: set AETHER_VFS_HOME, XDG_DATA_HOME, HOME, or (Windows) LOCALAPPDATA",
        ))
    }

    /// Builds a `Root` rooted at an explicit base directory. Used directly by
    /// tests, and by `from_env` once it has resolved the base.
    pub fn at(base: PathBuf) -> Root {
        Root { base }
    }

    /// Directory holding one subdirectory per extracted runtime.
    pub fn runtimes(&self) -> PathBuf {
        self.base.join("runtimes")
    }

    /// Directory holding downloaded (and not-yet-extracted) tarballs.
    pub fn downloads(&self) -> PathBuf {
        self.base.join("downloads")
    }

    /// Infallible convenience for literal, known-good tags (e.g. in tests or
    /// after `try_runtime_dir` has already validated the value). Callers
    /// handling a tag from a CLI argument or a GitHub release name must use
    /// [`Root::try_runtime_dir`] instead.
    pub fn runtime_dir(&self, tag: &str) -> PathBuf {
        self.runtimes().join(tag)
    }

    /// Validates `tag` before joining it under `runtimes()`. Rejects any tag
    /// that is empty, absolute, or contains a path separator or `..`
    /// component, since tags reach this from untrusted sources (a CLI
    /// argument, a GitHub release name) and must not be able to escape the
    /// runtimes directory.
    pub fn try_runtime_dir(&self, tag: &str) -> Result<PathBuf, InvalidTag> {
        let refuse = tag.is_empty()
            || tag.contains('/')
            || tag.contains('\\')
            || tag.contains("..")
            || Path::new(tag).is_absolute();
        if refuse {
            return Err(InvalidTag(tag.to_string()));
        }
        Ok(self.runtime_dir(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_places_everything_under_the_given_base() {
        let r = Root::at(std::path::PathBuf::from("/tmp/aebase"));
        assert_eq!(r.runtimes(), std::path::Path::new("/tmp/aebase/runtimes"));
        assert_eq!(
            r.runtime_dir("GE-Proton11-6"),
            std::path::Path::new("/tmp/aebase/runtimes/GE-Proton11-6")
        );
        assert_eq!(r.downloads(), std::path::Path::new("/tmp/aebase/downloads"));
    }

    #[test]
    fn a_tag_cannot_escape_the_runtimes_directory() {
        // Tags reach this from a CLI argument and from a GitHub release name, so
        // a traversal attempt must not resolve outside `runtimes()`.
        for evil in ["../../etc", "..", "a/../../b", "/absolute", "a/b"] {
            assert!(
                Root::at(std::path::PathBuf::from("/tmp/aebase"))
                    .try_runtime_dir(evil)
                    .is_err(),
                "tag {evil:?} must be refused"
            );
        }
        assert!(Root::at(std::path::PathBuf::from("/tmp/aebase"))
            .try_runtime_dir("GE-Proton11-6")
            .is_ok());
    }
}
