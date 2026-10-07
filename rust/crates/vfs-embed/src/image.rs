//! Where a launch image lives: in one of the session's roots, or outside all
//! of them.
//!
//! A root's **location** is the path the launched program sees — a host path
//! on Windows, a `C:\…` path inside the Wine prefix on Linux. Both are Windows
//! paths, so this module is pure string logic with Windows semantics on either
//! host: case-insensitive, `\` and `/` interchangeable, `\\?\` understood.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootLocation {
    pub id: u32,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageTarget {
    /// Inside `root`'s location, at `vpath` (`/`-separated) within it.
    InRoot { root: u32, vpath: String },
    /// Outside every root: a real program, launched as given.
    Outside(String),
}

/// `C:\…`/`C:/…`, `\\?\…` or a UNC name.
pub fn is_windows_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || s.starts_with("\\\\")
        || s.starts_with("//")
}

/// (lowercased volume, components) for an absolute Windows path. The volume is
/// `c:` for a drive path and `\\server\share` for UNC.
fn split_abs(s: &str) -> Option<(String, Vec<String>)> {
    let norm = s.replace('/', "\\");
    let norm = norm
        .strip_prefix(r"\\?\")
        .map(|rest| {
            rest.strip_prefix(r"UNC\")
                .map(|u| format!(r"\\{u}"))
                .unwrap_or_else(|| rest.to_string())
        })
        .unwrap_or(norm);
    let parts = |t: &str| -> Vec<String> {
        t.split('\\')
            .filter(|c| !c.is_empty() && *c != ".")
            .map(str::to_string)
            .collect()
    };
    let b = norm.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some((norm[..2].to_ascii_lowercase(), parts(&norm[2..])));
    }
    if let Some(unc) = norm.strip_prefix(r"\\") {
        let mut it = parts(unc).into_iter();
        let (server, share) = (it.next()?, it.next()?);
        return Some((
            format!(r"\\{server}\{share}").to_ascii_lowercase(),
            it.collect(),
        ));
    }
    None
}

fn has_dot_dot(s: &str) -> bool {
    s.split(['\\', '/']).any(|c| c == "..")
}

pub fn classify_image(image: &str, roots: &[RootLocation]) -> Result<ImageTarget, String> {
    let image = image.trim();
    if image.is_empty() {
        return Err("launch image is empty — name the image to launch".into());
    }
    if has_dot_dot(image) {
        return Err(format!(
            "launch image {image:?} contains '..'; name the image by a plain path inside a root"
        ));
    }
    if !is_windows_absolute(image) {
        let vpath: Vec<&str> = image
            .split(['\\', '/'])
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();
        if vpath.is_empty() {
            return Err(format!("launch image {image:?} names no file"));
        }
        return Ok(ImageTarget::InRoot {
            root: 0,
            vpath: vpath.join("/"),
        });
    }
    let (vol, comps) = split_abs(image)
        .ok_or_else(|| format!("launch image {image:?} is not a usable absolute path"))?;
    let mut best: Option<(usize, u32)> = None;
    for r in roots {
        let Some((rvol, rcomps)) = split_abs(&r.location) else {
            continue;
        };
        let inside = rvol == vol
            && rcomps.len() <= comps.len()
            && rcomps
                .iter()
                .zip(&comps)
                .all(|(a, b)| a.eq_ignore_ascii_case(b));
        if inside && best.is_none_or(|(n, _)| rcomps.len() > n) {
            best = Some((rcomps.len(), r.id));
        }
    }
    match best {
        None => Ok(ImageTarget::Outside(image.to_string())),
        Some((n, id)) => {
            let rest = &comps[n..];
            if rest.is_empty() {
                return Err(format!(
                    "launch image {image:?} is root {id}'s location itself, a directory — \
                     name a program inside it"
                ));
            }
            Ok(ImageTarget::InRoot {
                root: id,
                vpath: rest.join("/"),
            })
        }
    }
}

pub fn join_location(location: &str, vpath: &str) -> String {
    let base = location.trim_end_matches(['\\', '/']);
    format!(r"{base}\{}", vpath.replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<RootLocation> {
        vec![
            RootLocation {
                id: 0,
                location: r"C:\Games\Fixture".into(),
            },
            RootLocation {
                id: 1,
                location: r"C:\users\steamuser\Saves".into(),
            },
            // Nested inside root 0: the longest location must win.
            RootLocation {
                id: 2,
                location: r"C:\Games\Fixture\Data\Mods".into(),
            },
        ]
    }

    #[test]
    fn relative_is_root_zero() {
        assert_eq!(
            classify_image(r"bin\game.exe", &roots()).unwrap(),
            ImageTarget::InRoot {
                root: 0,
                vpath: "bin/game.exe".into()
            }
        );
    }

    #[test]
    fn absolute_inside_a_root_is_that_root() {
        assert_eq!(
            classify_image(r"C:\users\steamuser\Saves\tool.exe", &roots()).unwrap(),
            ImageTarget::InRoot {
                root: 1,
                vpath: "tool.exe".into()
            }
        );
    }

    #[test]
    fn matches_across_case_separators_and_trailing_slash() {
        let r = vec![RootLocation {
            id: 0,
            location: "c:/games/fixture/".into(),
        }];
        assert_eq!(
            classify_image(r"C:\GAMES\Fixture\x.exe", &r).unwrap(),
            ImageTarget::InRoot {
                root: 0,
                vpath: "x.exe".into()
            }
        );
    }

    #[test]
    fn nested_roots_pick_the_longest_location() {
        assert_eq!(
            classify_image(r"C:\Games\Fixture\Data\Mods\m.exe", &roots()).unwrap(),
            ImageTarget::InRoot {
                root: 2,
                vpath: "m.exe".into()
            }
        );
        assert_eq!(
            classify_image(r"C:\Games\Fixture\Data\other.exe", &roots()).unwrap(),
            ImageTarget::InRoot {
                root: 0,
                vpath: "Data/other.exe".into()
            }
        );
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_inside() {
        // `C:\Games\FixtureX` must not match root `C:\Games\Fixture`.
        assert_eq!(
            classify_image(r"C:\Games\FixtureX\a.exe", &roots()).unwrap(),
            ImageTarget::Outside(r"C:\Games\FixtureX\a.exe".into())
        );
    }

    #[test]
    fn outside_every_root_is_passed_through_verbatim() {
        assert_eq!(
            classify_image("C:/tools/probe.exe", &roots()).unwrap(),
            ImageTarget::Outside("C:/tools/probe.exe".into())
        );
    }

    #[test]
    fn verbatim_prefix_is_understood() {
        assert_eq!(
            classify_image(r"\\?\C:\Games\Fixture\g.exe", &roots()).unwrap(),
            ImageTarget::InRoot {
                root: 0,
                vpath: "g.exe".into()
            }
        );
    }

    #[test]
    fn the_root_itself_is_not_an_image() {
        let e = classify_image(r"C:\Games\Fixture\", &roots()).unwrap_err();
        assert!(e.contains("root 0") && e.contains("directory"), "{e}");
    }

    #[test]
    fn dot_dot_is_refused_anywhere() {
        for img in [
            r"..\x.exe",
            r"C:\Games\Fixture\..\..\Windows\x.exe",
            r"C:\a\..\b.exe",
        ] {
            let e = classify_image(img, &roots()).unwrap_err();
            assert!(e.contains(".."), "{img}: {e}");
        }
    }

    #[test]
    fn empty_is_refused() {
        assert!(classify_image("   ", &roots()).is_err());
    }

    #[test]
    fn join_location_uses_backslashes_once() {
        assert_eq!(
            join_location(r"C:\Games\Fixture", "bin/g.exe"),
            r"C:\Games\Fixture\bin\g.exe"
        );
        assert_eq!(
            join_location(r"C:\Games\Fixture\", "g.exe"),
            r"C:\Games\Fixture\g.exe"
        );
    }
}
