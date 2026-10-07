//! The overlay tree: a per-profile copy-on-write layer over the real registry.
//!
//! Nodes are keyed by the folded canonical path, so lookups are case-insensitive while each
//! node remembers its own spelling. A deleted key leaves a [`Child::Tombstone`] in its parent
//! and no node of its own; everything below it is dropped.
//!
//! Change tracking for `changed_since`: every node carries the version of its last own change
//! (values, existence or child list) and the newest change anywhere below it. Deleted keys keep
//! a record of the version they were deleted at.
use crate::path::{self, fold};
use std::collections::BTreeMap;

pub const MAX_KEY_NAME: usize = 255;
pub const MAX_VALUE_NAME: usize = 16383;
pub const MAX_DATA: usize = 1 << 20;
/// Deepest key nesting a new write may create, counted as components below `\Registry`
/// (Windows' own limit).
/// Most distinct deletion records kept for `changed_since`. On overflow they are all dropped
/// and `deleted_floor` rises to the newest of them: a spurious change notification for an
/// absent key, never a missed one.
pub const MAX_DELETED_AT: usize = 16384;
pub const MAX_WRITE_DEPTH: usize = 512;
/// Deepest key nesting a persisted overlay may rebuild with (`format::decode`, `insert_node`).
/// Higher than [`MAX_WRITE_DEPTH`] because the old writer had no cap, so files with keys up to
/// 1024 deep may exist and must keep loading; new writes are refused above 512.
pub const MAX_DECODE_DEPTH: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub name: String,
    pub ty: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Child {
    Present,
    Tombstone,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    /// In insertion order; lookups by `fold(name)`.
    pub values: Vec<Value>,
    /// Folded names of values deleted here.
    pub value_tombstones: Vec<String>,
    /// Folded child name -> (stored spelling, state).
    pub children: BTreeMap<String, (String, Child)>,
    /// Created here: no real counterpart is required (or allowed to show through).
    pub created: bool,
    pub volatile: bool,
    /// FILETIME.
    pub last_write: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    Absent,
    Present { created: bool },
    Tombstoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegError {
    NameTooLong,
    DataTooLarge,
    NotFound,
    AlreadyExists,
    InvalidPath,
    /// More than [`MAX_WRITE_DEPTH`] levels below `\Registry`.
    TooDeep,
}

impl std::fmt::Display for RegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RegError::NameTooLong => "name too long",
            RegError::DataTooLarge => "data too large",
            RegError::NotFound => "not found",
            RegError::AlreadyExists => "already exists",
            RegError::InvalidPath => "invalid path",
            RegError::TooDeep => "key nested too deeply",
        })
    }
}

impl std::error::Error for RegError {}

#[derive(Debug, Clone)]
struct Entry {
    /// Canonical path in stored spelling.
    path: String,
    node: Node,
    /// Version of the last change to this key itself.
    changed: u64,
    /// Newest change strictly below this key.
    subtree: u64,
}

#[derive(Debug, Default)]
pub struct Overlay {
    entries: BTreeMap<String, Entry>,
    /// Folded path of a tombstoned key -> version it was deleted (or renamed away) at.
    tombs: BTreeMap<String, u64>,
    /// Folded path -> newest version at which a key of that name was deleted or renamed away.
    /// Unlike `tombs` it survives revival of the path, so `changed_since` still sees the
    /// deletion of a former descendant after its ancestor is recreated or its name reused.
    deleted_at: BTreeMap<String, u64>,
    /// Deletions at or below this version have been forgotten (see [`MAX_DELETED_AT`]); an
    /// absent key then counts as changed for any older `version`.
    deleted_floor: u64,
    version: u64,
}

/// Length in UTF-16 code units, the unit the registry limits are in.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// A canonical, well-formed path: `\Registry` then non-empty components.
fn valid_path(p: &str) -> bool {
    let mut it = p.split('\\');
    it.next() == Some("")
        && it.next().is_some_and(|r| fold(r) == "registry")
        && it.all(|c| !c.is_empty())
}

fn check_key_name(p: &str) -> Result<(), RegError> {
    if p.split('\\').skip(1).any(|c| utf16_len(c) > MAX_KEY_NAME) {
        return Err(RegError::NameTooLong);
    }
    Ok(())
}

fn check_depth(p: &str, max: usize) -> Result<(), RegError> {
    // `p` is a valid path: the empty lead and `Registry` are not levels.
    if p.split('\\').count() - 2 > max {
        return Err(RegError::TooDeep);
    }
    Ok(())
}

fn child_prefix(folded: &str) -> String {
    format!("{folded}\\")
}

impl Overlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Restore the version counter after rebuilding from persisted nodes.
    pub fn set_version(&mut self, version: u64) {
        self.version = version;
    }

    /// State of `path`, and whether anything (a node or a tombstone) exists strictly below it.
    pub fn lookup(&self, path: &str) -> (Lookup, bool) {
        if !valid_path(path) {
            return (Lookup::Absent, false);
        }
        let f = fold(path);
        if let Some(e) = self.entries.get(&f) {
            return (
                Lookup::Present {
                    created: e.node.created,
                },
                self.any_below(&f),
            );
        }
        if self.is_tombstoned(&f) {
            return (Lookup::Tombstoned, false);
        }
        (Lookup::Absent, self.any_below(&f))
    }

    fn any_below(&self, folded: &str) -> bool {
        let pre = child_prefix(folded);
        self.entries
            .range(pre.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&pre))
            || self
                .tombs
                .range(pre.clone()..)
                .next()
                .is_some_and(|(k, _)| k.starts_with(&pre))
    }

    /// Tombstoned itself, or below a tombstoned ancestor.
    fn is_tombstoned(&self, folded: &str) -> bool {
        let mut cur = folded;
        loop {
            if self.tombs.contains_key(cur) {
                return true;
            }
            match path::parent(cur) {
                Some(p) => cur = p,
                None => return false,
            }
        }
    }

    pub fn node(&self, path: &str) -> Option<&Node> {
        self.entries.get(&fold(path)).map(|e| &e.node)
    }

    /// Every node with its canonical path (stored spelling), parents before children.
    pub fn walk(&self, mut f: impl FnMut(&str, &Node)) {
        for e in self.entries.values() {
            f(&e.path, &e.node);
        }
    }

    /// Insert a decoded node at `path` without touching the version; for rebuilding from a
    /// persisted layer. Insert parents first (as [`Overlay::walk`] yields them). Tombstoned
    /// children listed in `node` are recorded as changed at `changed`.
    pub fn insert_node(&mut self, path: &str, node: Node, changed: u64) -> Result<(), RegError> {
        if !valid_path(path) {
            return Err(RegError::InvalidPath);
        }
        check_depth(path, MAX_DECODE_DEPTH)?;
        let f = fold(path);
        for (cf, (_, st)) in &node.children {
            if *st == Child::Tombstone {
                self.tomb(format!("{f}\\{cf}"), changed);
            }
        }
        self.tombs.remove(&f);
        self.entries.insert(
            f,
            Entry {
                path: path.to_string(),
                node,
                changed,
                subtree: changed,
            },
        );
        Ok(())
    }

    pub fn set_value(
        &mut self,
        path: &str,
        name: &str,
        ty: u32,
        data: &[u8],
        now: u64,
    ) -> Result<u64, RegError> {
        if !valid_path(path) {
            return Err(RegError::InvalidPath);
        }
        check_key_name(path)?;
        check_depth(path, MAX_WRITE_DEPTH)?;
        if utf16_len(name) > MAX_VALUE_NAME {
            return Err(RegError::NameTooLong);
        }
        if data.len() > MAX_DATA {
            return Err(RegError::DataTooLarge);
        }
        let v = self.bump();
        self.ensure_chain(path, true, v, now);
        let fname = fold(name);
        let e = self.entries.get_mut(&fold(path)).expect("chain ensured");
        e.node.value_tombstones.retain(|t| *t != fname);
        match e.node.values.iter_mut().find(|x| fold(&x.name) == fname) {
            Some(x) => {
                x.ty = ty;
                x.data = data.to_vec();
            }
            None => e.node.values.push(Value {
                name: name.to_string(),
                ty,
                data: data.to_vec(),
            }),
        }
        e.node.last_write = now;
        self.mark(path, v);
        Ok(v)
    }

    /// Delete a value; the tombstone also hides a real value of that name. `NotFound` if the
    /// key itself is tombstoned.
    pub fn delete_value(&mut self, path: &str, name: &str, now: u64) -> Result<u64, RegError> {
        if !valid_path(path) {
            return Err(RegError::InvalidPath);
        }
        check_key_name(path)?;
        if utf16_len(name) > MAX_VALUE_NAME {
            return Err(RegError::NameTooLong);
        }
        if self.lookup(path).0 == Lookup::Tombstoned {
            return Err(RegError::NotFound);
        }
        let v = self.bump();
        self.ensure_chain(path, true, v, now);
        let fname = fold(name);
        let e = self.entries.get_mut(&fold(path)).expect("chain ensured");
        e.node.values.retain(|x| fold(&x.name) != fname);
        if !e.node.value_tombstones.contains(&fname) {
            e.node.value_tombstones.push(fname);
        }
        e.node.last_write = now;
        self.mark(path, v);
        Ok(v)
    }

    /// Create a key. `real_exists`: a real key of that name is visible, so the overlay node is
    /// a plain presence (not created-here). Over a tombstone it is always created-here.
    /// `AlreadyExists` if the overlay already holds the node.
    pub fn create_key(
        &mut self,
        path: &str,
        volatile: bool,
        real_exists: bool,
        now: u64,
    ) -> Result<u64, RegError> {
        if !valid_path(path) {
            return Err(RegError::InvalidPath);
        }
        check_key_name(path)?;
        check_depth(path, MAX_WRITE_DEPTH)?;
        if self.entries.contains_key(&fold(path)) {
            return Err(RegError::AlreadyExists);
        }
        let v = self.bump();
        self.ensure_chain(path, real_exists, v, now);
        self.entries
            .get_mut(&fold(path))
            .expect("chain ensured")
            .node
            .volatile = volatile;
        self.mark(path, v);
        Ok(v)
    }

    /// Tombstone the key and drop its subtree. `NotFound` if already tombstoned;
    /// `InvalidPath` for the root.
    pub fn delete_key(&mut self, path: &str, now: u64) -> Result<u64, RegError> {
        if !valid_path(path) {
            return Err(RegError::InvalidPath);
        }
        let parent = path::parent(path).ok_or(RegError::InvalidPath)?;
        if self.lookup(path).0 == Lookup::Tombstoned {
            return Err(RegError::NotFound);
        }
        let v = self.bump();
        self.ensure_chain(parent, true, v, now);
        let f = fold(path);
        self.remove_subtree(&f);
        self.tomb(f.clone(), v);
        let pe = self.entries.get_mut(&fold(parent)).expect("chain ensured");
        pe.node.children.insert(
            fold(path::leaf(path)),
            (path::leaf(path).to_string(), Child::Tombstone),
        );
        pe.node.last_write = now;
        pe.changed = v;
        self.touch_ancestors(parent, v);
        Ok(v)
    }

    /// Rename a key that exists in the overlay to `new_leaf` under the same parent, moving its
    /// subtree. The old name is tombstoned; the moved keys are created-here (the new name has
    /// no real counterpart).
    pub fn rename_key(&mut self, path: &str, new_leaf: &str, now: u64) -> Result<u64, RegError> {
        if !valid_path(path) {
            return Err(RegError::InvalidPath);
        }
        if new_leaf.is_empty() || new_leaf.contains('\\') {
            return Err(RegError::InvalidPath);
        }
        if utf16_len(new_leaf) > MAX_KEY_NAME {
            return Err(RegError::NameTooLong);
        }
        let parent = path::parent(path).ok_or(RegError::InvalidPath)?;
        let old_f = fold(path);
        let Some(root) = self.entries.get(&old_f) else {
            return Err(RegError::NotFound);
        };
        let old_stored = root.path.clone();
        let parent_stored = path::parent(&old_stored).expect("has parent").to_string();
        let new_path = format!("{parent_stored}\\{new_leaf}");
        let new_f = fold(&new_path);
        let case_only = new_f == old_f;
        if !case_only && self.entries.contains_key(&new_f) {
            return Err(RegError::AlreadyExists);
        }
        let v = self.bump();
        let moved_keys: Vec<String> = self.subtree_keys(&old_f);
        let moved_tombs: Vec<(String, u64)> = self
            .tombs
            .range(child_prefix(&old_f)..)
            .take_while(|(k, _)| k.starts_with(&child_prefix(&old_f)))
            .map(|(k, t)| (k.clone(), *t))
            .collect();
        let mut moved = Vec::new();
        for k in moved_keys {
            moved.push(self.entries.remove(&k).expect("listed"));
        }
        self.remove_subtree(&old_f);
        self.tombs.remove(&new_f);
        let rest = |k: &str, from: &str| k[from.len()..].to_string();
        for mut e in moved {
            let suffix = e.path[old_stored.len()..].to_string();
            e.path = format!("{new_path}{suffix}");
            e.node.created = true;
            e.changed = v;
            e.subtree = v;
            let key = format!(
                "{new_f}{}",
                rest(&fold(&format!("{old_stored}{suffix}")), &old_f)
            );
            self.entries.insert(key, e);
        }
        for (k, _) in moved_tombs {
            self.tomb(format!("{new_f}{}", rest(&k, &old_f)), v);
        }
        self.entries.get_mut(&new_f).expect("moved").node.last_write = now;
        let pe = self
            .entries
            .get_mut(&fold(parent))
            .expect("parent of a present node");
        if !case_only {
            pe.node.children.insert(
                fold(path::leaf(path)),
                (path::leaf(path).to_string(), Child::Tombstone),
            );
            self.tomb(old_f, v);
        }
        let pe = self.entries.get_mut(&fold(parent)).expect("parent");
        pe.node
            .children
            .insert(fold(new_leaf), (new_leaf.to_string(), Child::Present));
        pe.node.last_write = now;
        pe.changed = v;
        self.touch_ancestors(parent, v);
        Ok(v)
    }

    /// Whether `path` changed after `version`: its own values, existence or child list and, with
    /// `subtree`, anything below it. A key that has been deleted counts as changed by the
    /// deletion of it or of any ancestor.
    pub fn changed_since(&self, path: &str, subtree: bool, version: u64) -> bool {
        let f = fold(path);
        if let Some(e) = self.entries.get(&f) {
            return e.changed > version || (subtree && e.subtree > version);
        }
        let mut cur = f.as_str();
        loop {
            if self.deleted_at.get(cur).is_some_and(|t| *t > version) {
                return true;
            }
            match path::parent(cur) {
                Some(p) => cur = p,
                None => return self.deleted_floor > version,
            }
        }
    }

    /// Record a tombstone for `folded` at version `v` (and the surviving deletion mark).
    fn tomb(&mut self, folded: String, v: u64) {
        let hw = self.deleted_at.entry(folded.clone()).or_insert(0);
        *hw = (*hw).max(v);
        self.tombs.insert(folded, v);
        if self.deleted_at.len() > MAX_DELETED_AT {
            let newest = self.deleted_at.values().copied().max().unwrap_or(0);
            self.deleted_floor = self.deleted_floor.max(newest);
            self.deleted_at.clear();
        }
    }

    fn bump(&mut self) -> u64 {
        self.version += 1;
        self.version
    }

    /// Record a change at `path` (it exists): own change plus subtree change on ancestors.
    fn mark(&mut self, path: &str, v: u64) {
        if let Some(e) = self.entries.get_mut(&fold(path)) {
            e.changed = v;
        }
        self.touch_ancestors(path, v);
    }

    fn touch_ancestors(&mut self, path: &str, v: u64) {
        let mut cur = path::parent(path);
        while let Some(p) = cur {
            if let Some(e) = self.entries.get_mut(&fold(p)) {
                e.subtree = v;
            }
            cur = path::parent(p);
        }
    }

    fn subtree_keys(&self, folded: &str) -> Vec<String> {
        let pre = child_prefix(folded);
        let mut keys: Vec<String> = Vec::new();
        if self.entries.contains_key(folded) {
            keys.push(folded.to_string());
        }
        keys.extend(
            self.entries
                .range(pre.clone()..)
                .take_while(|(k, _)| k.starts_with(&pre))
                .map(|(k, _)| k.clone()),
        );
        keys
    }

    /// Drop the node at `folded`, everything below it, and their tombstone records.
    fn remove_subtree(&mut self, folded: &str) {
        for k in self.subtree_keys(folded) {
            self.entries.remove(&k);
        }
        let pre = child_prefix(folded);
        let tomb_keys: Vec<String> = self
            .tombs
            .range(pre.clone()..)
            .take_while(|(k, _)| k.starts_with(&pre))
            .map(|(k, _)| k.clone())
            .collect();
        for k in tomb_keys {
            self.tombs.remove(&k);
        }
        self.tombs.remove(folded);
    }

    /// Make every key on the way to `path` (inclusive) exist as a node. A missing key under a
    /// created-here parent, or one that was tombstoned (the chain is being revived), is
    /// created-here; otherwise it is a plain presence, except the target itself when
    /// `target_real_exists` is false. New nodes get their parent's child list updated.
    fn ensure_chain(&mut self, path: &str, target_real_exists: bool, v: u64, now: u64) {
        let comps: Vec<&str> = path.split('\\').skip(1).collect();
        let mut stored = String::new();
        let mut parent_key: Option<String> = None;
        for (i, comp) in comps.iter().enumerate() {
            let is_target = i + 1 == comps.len();
            stored = format!("{stored}\\{comp}");
            let key = fold(&stored);
            if self.entries.contains_key(&key) {
                // Keep the stored spelling already in use for the rest of the chain.
                stored = self.entries[&key].path.clone();
                parent_key = Some(key);
                continue;
            }
            let revived = self.tombs.remove(&key).is_some()
                || parent_key.as_ref().is_some_and(|pk| {
                    self.entries[pk]
                        .node
                        .children
                        .get(&fold(comp))
                        .is_some_and(|c| c.1 == Child::Tombstone)
                });
            let parent_created = parent_key
                .as_ref()
                .is_some_and(|pk| self.entries[pk].node.created);
            let created = revived || parent_created || (is_target && !target_real_exists);
            let node = Node {
                created,
                last_write: now,
                ..Node::default()
            };
            self.entries.insert(
                key.clone(),
                Entry {
                    path: stored.clone(),
                    node,
                    changed: v,
                    subtree: 0,
                },
            );
            if let Some(pk) = &parent_key {
                let pe = self.entries.get_mut(pk).expect("parent");
                pe.node
                    .children
                    .insert(fold(comp), (comp.to_string(), Child::Present));
                pe.node.last_write = now;
                pe.changed = v;
            }
            parent_key = Some(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: &str = r"\Registry\Machine\Software\Mod";
    const REG_SZ: u32 = 1;

    fn val<'a>(o: &'a Overlay, p: &str, n: &str) -> Option<&'a Value> {
        let f = n.to_lowercase();
        o.node(p)?
            .values
            .iter()
            .find(|v| v.name.to_lowercase() == f)
    }

    #[test]
    fn depth_cap_refuses_513_and_accepts_512() {
        let chain = |n: usize| {
            let mut p = String::from(r"\Registry");
            for i in 0..n {
                p.push_str(&format!(r"\k{i}"));
            }
            p
        };
        let mut o = Overlay::new();
        assert!(
            o.create_key(&chain(MAX_WRITE_DEPTH), false, false, 1)
                .is_ok()
        );
        let deep = chain(MAX_WRITE_DEPTH + 1);
        assert_eq!(o.create_key(&deep, false, false, 2), Err(RegError::TooDeep));
        assert_eq!(
            o.set_value(&deep, "v", REG_SZ, b"a", 2),
            Err(RegError::TooDeep)
        );
        // The rebuild path accepts what the old writer could have saved, up to the decode cap.
        assert!(o.insert_node(&deep, Node::default(), 2).is_ok());
        let too_deep = chain(MAX_DECODE_DEPTH + 1);
        assert_eq!(
            o.insert_node(&too_deep, Node::default(), 2),
            Err(RegError::TooDeep)
        );
        // The refused writes left the version alone.
        assert_eq!(o.version(), 1);
    }

    #[test]
    fn deleted_at_is_bounded_and_stays_conservative() {
        let mut o = Overlay::new();
        let base = r"\Registry\Machine\Software";
        o.create_key(base, false, false, 1).unwrap();
        for i in 0..MAX_DELETED_AT + 5 {
            let p = format!(r"{base}\k{i}");
            o.create_key(&p, false, false, 1).unwrap();
            o.delete_key(&p, 1).unwrap();
        }
        assert!(o.deleted_at.len() <= MAX_DELETED_AT);
        let v = o.version();
        // A key deleted before the floor still reads as changed for older versions...
        assert!(o.changed_since(&format!(r"{base}\k0"), false, 0));
        // ...and nothing is reported changed at or after the newest version.
        assert!(!o.changed_since(&format!(r"{base}\never"), false, v));
    }

    #[test]
    fn lookup_is_case_insensitive_and_spelling_preserved() {
        let mut o = Overlay::new();
        o.create_key(K, false, false, 1).unwrap();
        let (l, _) = o.lookup(&K.to_uppercase());
        assert_eq!(l, Lookup::Present { created: true });
        assert!(o.node(&K.to_lowercase()).is_some());
        let parent = o.node(r"\Registry\Machine\Software").unwrap();
        assert_eq!(parent.children["mod"], ("Mod".to_string(), Child::Present));
        o.set_value(&K.to_lowercase(), "MyVal", REG_SZ, b"a", 2)
            .unwrap();
        o.set_value(K, "MYVAL", REG_SZ, b"b", 3).unwrap();
        let n = o.node(K).unwrap();
        assert_eq!(n.values.len(), 1);
        assert_eq!(n.values[0].name, "MyVal");
        assert_eq!(n.values[0].data, b"b");
        assert_eq!(n.last_write, 3);
    }

    #[test]
    fn missing_ancestors_are_present_not_created() {
        let mut o = Overlay::new();
        o.set_value(K, "v", REG_SZ, b"x", 1).unwrap();
        assert_eq!(
            o.lookup(r"\Registry\Machine\Software").0,
            Lookup::Present { created: false }
        );
        assert_eq!(o.lookup(r"\Registry").0, Lookup::Present { created: false });
        assert_eq!(o.lookup(K).0, Lookup::Present { created: false });
        assert!(o.lookup(r"\Registry\Machine\Software").1);
        assert!(!o.lookup(K).1);
        assert_eq!(
            o.lookup(r"\Registry\Machine\Other"),
            (Lookup::Absent, false)
        );
    }

    #[test]
    fn set_delete_set_same_value() {
        let mut o = Overlay::new();
        o.set_value(K, "V", REG_SZ, b"1", 1).unwrap();
        o.delete_value(K, "v", 2).unwrap();
        assert!(val(&o, K, "V").is_none());
        assert_eq!(o.node(K).unwrap().value_tombstones, vec!["v".to_string()]);
        o.set_value(K, "V", REG_SZ, b"2", 3).unwrap();
        assert_eq!(val(&o, K, "V").unwrap().data, b"2");
        assert!(o.node(K).unwrap().value_tombstones.is_empty());
    }

    #[test]
    fn delete_value_with_no_overlay_copy_still_hides_real() {
        let mut o = Overlay::new();
        o.delete_value(K, "Real", 1).unwrap();
        assert_eq!(
            o.node(K).unwrap().value_tombstones,
            vec!["real".to_string()]
        );
    }

    #[test]
    fn delete_key_tombstones_and_drops_subtree() {
        let mut o = Overlay::new();
        let sub = format!(r"{K}\Sub\Deep");
        o.set_value(&sub, "v", REG_SZ, b"x", 1).unwrap();
        o.delete_key(K, 2).unwrap();
        assert_eq!(o.lookup(K), (Lookup::Tombstoned, false));
        assert!(o.node(K).is_none());
        assert!(o.node(&sub).is_none());
        assert!(o.node(&format!(r"{K}\Sub")).is_none());
        assert_eq!(o.lookup(&sub).0, Lookup::Tombstoned);
        let p = o.node(r"\Registry\Machine\Software").unwrap();
        assert_eq!(p.children["mod"], ("Mod".to_string(), Child::Tombstone));
        assert_eq!(o.delete_key(K, 3), Err(RegError::NotFound));
        assert_eq!(o.delete_key(r"\Registry", 3), Err(RegError::InvalidPath));
        let mut n = 0;
        o.walk(|p, _| {
            assert!(!p.to_lowercase().contains(r"\mod"), "{p}");
            n += 1;
        });
        assert!(n > 0);
    }

    #[test]
    fn writing_under_tombstone_revives_the_chain() {
        let mut o = Overlay::new();
        o.set_value(K, "old", REG_SZ, b"x", 1).unwrap();
        o.delete_key(K, 2).unwrap();
        let deep = format!(r"{K}\Sub");
        o.set_value(&deep, "v", REG_SZ, b"y", 3).unwrap();
        assert_eq!(o.lookup(K).0, Lookup::Present { created: true });
        assert_eq!(o.lookup(&deep).0, Lookup::Present { created: true });
        assert!(val(&o, K, "old").is_none());
        let p = o.node(r"\Registry\Machine\Software").unwrap();
        assert_eq!(p.children["mod"].1, Child::Present);
        // create_key over a tombstone also revives.
        o.delete_key(K, 4).unwrap();
        o.create_key(K, true, true, 5).unwrap();
        assert_eq!(o.lookup(K).0, Lookup::Present { created: true });
        assert!(o.node(K).unwrap().volatile);
    }

    #[test]
    fn create_key_existing_is_already_exists() {
        let mut o = Overlay::new();
        o.create_key(K, false, true, 1).unwrap();
        assert_eq!(o.lookup(K).0, Lookup::Present { created: false });
        let v = o.version();
        assert_eq!(
            o.create_key(K, false, true, 2),
            Err(RegError::AlreadyExists)
        );
        assert_eq!(o.version(), v);
    }

    #[test]
    fn limits() {
        let mut o = Overlay::new();
        let ok_key = "k".repeat(255);
        o.create_key(&format!(r"{K}\{ok_key}"), false, false, 1)
            .unwrap();
        let long_key = "k".repeat(256);
        assert_eq!(
            o.create_key(&format!(r"{K}\{long_key}"), false, false, 1),
            Err(RegError::NameTooLong)
        );
        o.set_value(K, &"n".repeat(16383), REG_SZ, b"", 1).unwrap();
        assert_eq!(
            o.set_value(K, &"n".repeat(16384), REG_SZ, b"", 1),
            Err(RegError::NameTooLong)
        );
        assert_eq!(
            o.delete_value(K, &"n".repeat(16384), 1),
            Err(RegError::NameTooLong)
        );
        o.set_value(K, "big", REG_SZ, &vec![0; 1 << 20], 1).unwrap();
        assert_eq!(
            o.set_value(K, "big", REG_SZ, &vec![0; (1 << 20) + 1], 1),
            Err(RegError::DataTooLarge)
        );
        assert_eq!(val(&o, K, "big").unwrap().data.len(), 1 << 20);
        assert_eq!(o.rename_key(K, &long_key, 1), Err(RegError::NameTooLong));
        assert_eq!(o.rename_key(K, r"a\b", 1), Err(RegError::InvalidPath));
        assert_eq!(
            o.set_value(r"\Device\x", "v", REG_SZ, b"", 1),
            Err(RegError::InvalidPath)
        );
    }

    #[test]
    fn version_bumps_once_per_mutation() {
        let mut o = Overlay::new();
        assert_eq!(o.version(), 0);
        // Deep chain creation is still one mutation.
        assert_eq!(o.set_value(K, "a", REG_SZ, b"", 1).unwrap(), 1);
        assert_eq!(o.delete_value(K, "a", 2).unwrap(), 2);
        assert_eq!(
            o.create_key(&format!(r"{K}\C"), false, false, 3).unwrap(),
            3
        );
        assert_eq!(o.rename_key(&format!(r"{K}\C"), "D", 4).unwrap(), 4);
        assert_eq!(o.delete_key(&format!(r"{K}\D"), 5).unwrap(), 5);
        assert_eq!(o.version(), 5);
        // A failed mutation does not bump.
        assert!(o.rename_key(&format!(r"{K}\Nope"), "X", 6).is_err());
        assert_eq!(o.version(), 5);
    }

    #[test]
    fn rename_moves_subtree() {
        let mut o = Overlay::new();
        let a = format!(r"{K}\A");
        o.set_value(&format!(r"{a}\Child"), "v", REG_SZ, b"x", 1)
            .unwrap();
        o.set_value(&a, "top", REG_SZ, b"t", 1).unwrap();
        o.rename_key(&a, "B", 2).unwrap();
        assert_eq!(o.lookup(&a).0, Lookup::Tombstoned);
        assert!(o.node(&format!(r"{K}\B")).is_some());
        assert_eq!(val(&o, &format!(r"{K}\B\Child"), "v").unwrap().data, b"x");
        assert_eq!(
            o.lookup(&format!(r"{K}\B")).0,
            Lookup::Present { created: true }
        );
        let p = o.node(K).unwrap();
        assert_eq!(p.children["a"].1, Child::Tombstone);
        assert_eq!(p.children["b"], ("B".to_string(), Child::Present));
        // Onto an existing overlay key.
        o.create_key(&format!(r"{K}\E"), false, false, 3).unwrap();
        assert_eq!(
            o.rename_key(&format!(r"{K}\B"), "e", 4),
            Err(RegError::AlreadyExists)
        );
        assert_eq!(o.rename_key(&a, "Z", 4), Err(RegError::NotFound));
        // Case-only rename respells without a tombstone.
        o.rename_key(&format!(r"{K}\E"), "e2", 5).unwrap();
        o.rename_key(&format!(r"{K}\e2"), "E2", 6).unwrap();
        assert_eq!(
            o.node(K).unwrap().children["e2"],
            ("E2".to_string(), Child::Present)
        );
        assert!(o.node(&format!(r"{K}\e2")).is_some());
    }

    #[test]
    fn changed_since_subtree_and_non_subtree() {
        let mut o = Overlay::new();
        let deep = format!(r"{K}\Sub\Deep");
        let v1 = o.set_value(&deep, "v", REG_SZ, b"x", 1).unwrap();
        // The leaf itself and its creation (parent's child list) changed.
        assert!(o.changed_since(&deep, false, 0));
        assert!(!o.changed_since(&deep, false, v1));
        // Parent gained a child at v1 (non-subtree sees it); grandparent only via subtree.
        assert!(o.changed_since(&format!(r"{K}\Sub"), false, 0));
        assert!(o.changed_since(K, false, 0)); // K's child list gained Sub
        let v2 = o.set_value(&deep, "w", REG_SZ, b"y", 2).unwrap();
        assert!(!o.changed_since(K, false, v1));
        assert!(o.changed_since(K, true, v1));
        assert!(o.changed_since(r"\Registry\Machine", true, v1));
        assert!(!o.changed_since(r"\Registry\Machine", false, v1));
        assert!(!o.changed_since(K, true, v2));
        assert!(!o.changed_since(r"\Registry\Machine\Nothing", true, 0));
        // Deletion: key, its parent, and watchers on dropped descendants.
        let v3 = o.delete_key(&format!(r"{K}\Sub"), 3).unwrap();
        assert!(o.changed_since(&format!(r"{K}\Sub"), false, v2));
        assert!(o.changed_since(K, false, v2));
        assert!(o.changed_since(&deep, true, v2));
        assert!(!o.changed_since(&deep, true, v3));
        // Value deletion changes only that key (non-subtree).
        let v4 = o.delete_value(K, "x", 4).unwrap();
        assert!(o.changed_since(K, false, v3));
        assert!(!o.changed_since(K, false, v4));
    }

    #[test]
    fn changed_since_sees_deletions_after_revive() {
        let mut o = Overlay::new();
        let sub = format!(r"{K}\Sub");
        let v1 = o.set_value(&sub, "v", REG_SZ, b"x", 1).unwrap();
        let v2 = o.delete_key(K, 2).unwrap();
        // Plain revive through set_value, then through create_key.
        let v3 = o.set_value(K, "n", REG_SZ, b"y", 3).unwrap();
        assert_eq!(o.lookup(&sub).0, Lookup::Absent);
        assert!(o.changed_since(&sub, false, v1));
        assert!(o.changed_since(&sub, true, v1));
        assert!(o.changed_since(K, false, v2));
        assert!(!o.changed_since(&sub, false, v3));
        let v4 = o.delete_key(K, 4).unwrap();
        o.create_key(K, false, true, 5).unwrap();
        assert!(o.changed_since(&sub, true, v3));
        assert!(!o.changed_since(&sub, true, v4));
    }

    #[test]
    fn changed_since_sees_old_name_after_rename_and_reuse() {
        let mut o = Overlay::new();
        let a = format!(r"{K}\A");
        let deep = format!(r"{a}\Deep");
        let v1 = o.set_value(&deep, "v", REG_SZ, b"x", 1).unwrap();
        let v2 = o.rename_key(&a, "B", 2).unwrap();
        // Reuse the old name.
        let v3 = o.set_value(&a, "w", REG_SZ, b"y", 3).unwrap();
        assert!(o.changed_since(&deep, true, v1));
        assert!(o.changed_since(&a, false, v2));
        assert!(!o.changed_since(&deep, true, v3));
        assert!(o.changed_since(&format!(r"{K}\B"), false, v1));
    }

    #[test]
    fn walk_and_rebuild_roundtrip() {
        let mut o = Overlay::new();
        o.set_value(K, "v", REG_SZ, b"x", 1).unwrap();
        o.create_key(&format!(r"{K}\Sub"), true, false, 2).unwrap();
        o.delete_key(&format!(r"{K}\Gone"), 3).unwrap();
        let mut seen: Vec<(String, Node)> = Vec::new();
        o.walk(|p, n| seen.push((p.to_string(), n.clone())));
        // Parents come before children.
        let idx = |p: &str| seen.iter().position(|(q, _)| q == p).unwrap();
        assert!(idx(r"\Registry") < idx(r"\Registry\Machine"));
        assert!(idx(r"\Registry\Machine\Software") < idx(K));
        assert!(idx(K) < idx(&format!(r"{K}\Sub")));
        let mut r = Overlay::new();
        for (p, n) in &seen {
            r.insert_node(p, n.clone(), 0).unwrap();
        }
        r.set_version(o.version());
        assert_eq!(r.version(), 3);
        assert_eq!(r.node(K), o.node(K));
        assert_eq!(r.lookup(&format!(r"{K}\Gone")).0, Lookup::Tombstoned);
        let mut again = Vec::new();
        r.walk(|p, n| again.push((p.to_string(), n.clone())));
        assert_eq!(again, seen);
        assert_eq!(
            r.insert_node(r"\Nope", Node::default(), 0),
            Err(RegError::InvalidPath)
        );
    }
}
