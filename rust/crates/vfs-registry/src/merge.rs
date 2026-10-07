//! The merged view: what a process sees at a key, from the real key plus the overlay node.
use crate::overlay::{Child, Node, Value};
use crate::path::fold;
use std::collections::HashSet;

/// A key as seen at one point of the merge: the real key as the shim read it (through the
/// unhooked calls) or the merged result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyView {
    pub subkeys: Vec<String>,
    pub values: Vec<Value>,
    pub class: Option<Vec<u16>>,
    pub last_write: u64,
    /// `MaxClassLen` (bytes): the longest subkey class. For a real key it is read from the real
    /// key's `KEY_FULL_INFORMATION` (subkey classes are not otherwise visible to the merge). For
    /// a merged key, overlay-created keys have no class, so it is the real key's value when the
    /// real key shows through, else 0. Like Windows (which keeps it as a high-water mark), it is
    /// not lowered when a real subkey is hidden.
    pub max_subkey_class_len: u32,
}

pub fn merge(real: Option<&KeyView>, node: Option<&Node>, tombstoned: bool) -> Option<KeyView> {
    if tombstoned || (real.is_none() && node.is_none()) {
        return None;
    }
    // A key created in the overlay has no real counterpart allowed to show through.
    let real = real.filter(|_| !node.is_some_and(|n| n.created));
    let mut out = KeyView {
        class: real.and_then(|r| r.class.clone()),
        last_write: real.map_or(0, |r| r.last_write),
        max_subkey_class_len: real.map_or(0, |r| r.max_subkey_class_len),
        ..KeyView::default()
    };
    let Some(node) = node else {
        let r = real?;
        out.subkeys = r.subkeys.clone();
        out.values = r.values.clone();
        return Some(out);
    };
    out.last_write = out.last_write.max(node.last_write);

    out.values = node.values.clone();
    if let Some(r) = real {
        let hidden: HashSet<String> = node
            .values
            .iter()
            .map(|v| fold(&v.name))
            .chain(node.value_tombstones.iter().cloned())
            .collect();
        out.values.extend(
            r.values
                .iter()
                .filter(|v| !hidden.contains(&fold(&v.name)))
                .cloned(),
        );
    }

    let mut real_names: HashSet<String> = HashSet::new();
    if let Some(r) = real {
        for s in &r.subkeys {
            let f = fold(s);
            let dead = matches!(node.children.get(&f), Some((_, Child::Tombstone)));
            real_names.insert(f);
            if !dead {
                out.subkeys.push(s.clone());
            }
        }
    }
    // BTreeMap keys are folded names, so this is already case-insensitive order.
    out.subkeys.extend(
        node.children
            .iter()
            .filter(|(f, (_, c))| *c == Child::Present && !real_names.contains(*f))
            .map(|(_, (spelling, _))| spelling.clone()),
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::Overlay;

    const P: &str = r"\Registry\Machine\Software\Mod";
    const SZ: u32 = 1;

    fn v(name: &str, data: &[u8]) -> Value {
        Value {
            name: name.into(),
            ty: SZ,
            data: data.into(),
        }
    }
    fn real(subs: &[&str], vals: Vec<Value>, lw: u64) -> KeyView {
        KeyView {
            subkeys: subs.iter().map(|s| s.to_string()).collect(),
            values: vals,
            class: Some(vec![1, 2]),
            last_write: lw,
            max_subkey_class_len: 6,
        }
    }

    #[test]
    fn both_absent_or_tombstoned_is_none() {
        assert!(merge(None, None, false).is_none());
        let r = real(&["a"], vec![], 5);
        assert!(merge(Some(&r), None, true).is_none());
        assert!(merge(Some(&r), Some(&Node::default()), true).is_none());
    }

    #[test]
    fn real_only_passes_through() {
        let r = real(&["b", "a"], vec![v("X", b"1")], 5);
        let m = merge(Some(&r), None, false).unwrap();
        assert_eq!(m.subkeys, vec!["b", "a"]);
        assert_eq!(m.values, r.values);
        assert_eq!(m.class, Some(vec![1, 2]));
        assert_eq!(m.last_write, 5);
        assert_eq!(m.max_subkey_class_len, 6);
    }

    #[test]
    fn overlay_only_node() {
        let mut o = Overlay::new();
        o.create_key(P, false, false, 7).unwrap();
        o.set_value(P, "V", SZ, b"z", 8).unwrap();
        let m = merge(None, o.node(P), false).unwrap();
        assert_eq!(m.values, vec![v("V", b"z")]);
        assert!(m.subkeys.is_empty());
        assert_eq!(m.class, None);
        assert_eq!(m.last_write, 8);
    }

    #[test]
    fn values_overlay_first_then_real_minus_shadowed_and_tombstoned() {
        let mut o = Overlay::new();
        o.set_value(P, "B", SZ, b"new", 10).unwrap();
        o.set_value(P, "N", SZ, b"n", 11).unwrap();
        o.delete_value(P, "c", 12).unwrap();
        let r = real(&[], vec![v("a", b"1"), v("b", b"2"), v("C", b"3")], 3);
        let m = merge(Some(&r), o.node(P), false).unwrap();
        let names: Vec<&str> = m.values.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["B", "N", "a"]);
        assert_eq!(m.values[0].data, b"new");
    }

    #[test]
    fn subkeys_real_order_minus_tombstones_then_created_sorted() {
        let mut o = Overlay::new();
        o.create_key(&format!(r"{P}\zeta"), false, false, 1)
            .unwrap();
        o.create_key(&format!(r"{P}\Alpha"), false, false, 2)
            .unwrap();
        o.create_key(&format!(r"{P}\mid"), false, false, 3).unwrap();
        o.delete_key(&format!(r"{P}\GONE"), 4).unwrap();
        let r = real(&["Gone", "Zed", "bee"], vec![], 0);
        let m = merge(Some(&r), o.node(P), false).unwrap();
        assert_eq!(m.subkeys, vec!["Zed", "bee", "Alpha", "mid", "zeta"]);
    }

    #[test]
    fn intermediate_node_does_not_duplicate_real_children() {
        // A deeper key was created under a real child: the child's node is present with
        // created == false and must not be listed twice, nor make absent real keys exist.
        let mut o = Overlay::new();
        o.create_key(&format!(r"{P}\Real\Deep"), false, false, 1)
            .unwrap();
        let r = real(&["real"], vec![], 0);
        let m = merge(Some(&r), o.node(P), false).unwrap();
        assert_eq!(m.subkeys, vec!["real"]);
        // created=false node with no real key: exists only because it is in the overlay.
        let n = o.node(&format!(r"{P}\Real")).unwrap();
        assert!(merge(None, Some(n), false).is_some());
    }

    #[test]
    fn last_write_is_the_later() {
        let mut o = Overlay::new();
        o.set_value(P, "V", SZ, b"1", 50).unwrap();
        let r = real(&[], vec![], 9);
        assert_eq!(merge(Some(&r), o.node(P), false).unwrap().last_write, 50);
        let r = real(&[], vec![], 99);
        assert_eq!(merge(Some(&r), o.node(P), false).unwrap().last_write, 99);
    }

    #[test]
    fn created_node_hides_real_contents() {
        let mut o = Overlay::new();
        o.create_key(P, false, false, 5).unwrap();
        let r = real(&["old"], vec![v("x", b"1")], 99);
        let m = merge(Some(&r), o.node(P), false).unwrap();
        assert!(m.subkeys.is_empty() && m.values.is_empty());
        assert_eq!(m.class, None);
        assert_eq!(m.last_write, 5);
        assert_eq!(m.max_subkey_class_len, 0);
    }

    #[test]
    fn order_is_stable_across_calls() {
        let mut o = Overlay::new();
        for n in ["q", "B", "m", "a"] {
            o.create_key(&format!(r"{P}\{n}"), false, false, 1).unwrap();
        }
        let r = real(&["z", "y"], vec![v("1", b""), v("2", b"")], 1);
        let first = merge(Some(&r), o.node(P), false).unwrap();
        for _ in 0..5 {
            assert_eq!(merge(Some(&r), o.node(P), false).unwrap(), first);
        }
    }

    /// Review Focus 1: NtEnumerateKey(i) with an increasing index while keys are deleted
    /// through the overlay. The model is the real registry: deleting a key shifts later keys
    /// down by one, so a caller that deletes the key it just saw re-reads the same index.
    #[test]
    fn enumerate_by_index_while_deleting_sees_each_survivor_once() {
        let names = ["k0", "K1", "k2", "k3", "k4", "k5", "k6", "k7"];
        for delete_pattern in 0u32..(1 << names.len()) {
            let mut o = Overlay::new();
            // Keys live in the real registry; the overlay only records tombstones.
            let r = real(&names, vec![], 1);
            let mut seen: Vec<String> = vec![];
            let mut i = 0;
            let mut now = 10;
            loop {
                let m = merge(Some(&r), o.node(P), false);
                let subs = m.map(|m| m.subkeys).unwrap_or_else(|| r.subkeys.clone());
                let Some(name) = subs.get(i) else { break };
                seen.push(name.clone());
                let idx = names.iter().position(|n| n == name).unwrap();
                if delete_pattern & (1 << idx) != 0 {
                    now += 1;
                    o.delete_key(&format!(r"{P}\{}", name.to_uppercase()), now)
                        .unwrap();
                    // Deleted: later keys shift down, so the index stays.
                } else {
                    i += 1;
                }
            }
            let mut sorted = seen.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                seen.len(),
                "duplicate in pattern {delete_pattern:b}"
            );
            assert_eq!(
                seen.len(),
                names.len(),
                "missed a key in pattern {delete_pattern:b}"
            );
            let survivors: Vec<&str> = names
                .iter()
                .enumerate()
                .filter(|(k, _)| delete_pattern & (1 << k) == 0)
                .map(|(_, n)| *n)
                .collect();
            let m = merge(Some(&r), o.node(P), false).unwrap_or_default();
            assert_eq!(m.subkeys, survivors);
        }
    }

    #[test]
    fn enumerate_with_later_key_deleted_skips_it() {
        let mut o = Overlay::new();
        let r = real(&["a", "b", "c", "d"], vec![], 1);
        let first = merge(Some(&r), o.node(P), false).unwrap();
        assert_eq!(first.subkeys[0], "a");
        o.delete_key(&format!(r"{P}\c"), 2).unwrap();
        let m = merge(Some(&r), o.node(P), false).unwrap();
        assert_eq!(m.subkeys, vec!["a", "b", "d"]);
        assert_eq!(m.subkeys.iter().filter(|s| *s == "d").count(), 1);
    }
}
