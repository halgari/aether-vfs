//! A layer under an overlay: copy-up and the spelling of names.

use super::*;

#[test]
fn overlay_copy_up_into_a_layer() {
    let (s, _d) = temp_storage();
    let upper = s.layer("upper").unwrap();
    let base: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
    let ov: Arc<dyn Provider> = Arc::new(
        vfs_compose::OverlayProvider::from_arcs(Arc::clone(&base), Arc::clone(&upper)).unwrap(),
    );

    // Root file, and one under a directory only the base has: the upper has
    // no `sub`, so the copy-up must be able to create it.
    for (rel, body) in [("a.txt", &b"hello"[..]), ("sub/b.txt", &b"world!"[..])] {
        let (h, _, _) = ov.open(at(rel), OPEN_WRITE).unwrap();
        ov.write_at(h, 0, b"J").unwrap();
        ov.close(h).unwrap();
        let mut want = body.to_vec();
        want[0] = b'J';
        assert_eq!(read_file(&ov, rel), want, "{rel} through the overlay");
        assert_eq!(read_file(&upper, rel), want, "{rel} in the layer");
        assert_eq!(
            read_file(&base, rel),
            body,
            "{rel} in the base is untouched"
        );
    }

    // A removal is a whiteout in the layer.
    ov.remove(at("a.txt")).unwrap();
    assert!(ov.getattr(at("a.txt")).unwrap().is_none());
    assert!(upper.getattr(at(".wh.a.txt")).unwrap().is_some());
}

#[test]
fn an_overlay_over_a_layer_passes_conformance() {
    let (s, _d) = temp_storage();
    let upper = s.layer("upper").unwrap();
    let base: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
    let ov = vfs_compose::OverlayProvider::from_arcs(base, upper).unwrap();
    vfs_provider::assert_conformance(Arc::new(ov));
}

// ---- names: what a listing and a final path spell ---------------------

/// A base with `Data/Interface/Fonts/a.ttf` and `Data/Skyrim.ini`, under
/// an overlay whose upper is a layer: what a game's root is.
pub(super) fn game_overlay(s: &Arc<Storage>) -> (Arc<dyn Provider>, Arc<dyn Provider>) {
    let base: Arc<dyn Provider> = Arc::new(vfs_compose::MemoryProvider::new());
    base.mkdir(at("Data")).unwrap();
    base.mkdir(at("Data/Interface")).unwrap();
    base.mkdir(at("Data/Interface/Fonts")).unwrap();
    write_file(&base, "Data/Interface/Fonts/a.ttf", 0, b"font");
    write_file(&base, "Data/Skyrim.ini", 0, b"[General]");
    let upper = s.layer("write").unwrap();
    let ov: Arc<dyn Provider> =
        Arc::new(vfs_compose::OverlayProvider::from_arcs(base, Arc::clone(&upper)).unwrap());
    (ov, upper)
}

pub(super) fn listed(p: &Arc<dyn Provider>, dir: &str) -> Vec<String> {
    p.readdir(at(dir))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// How `p` spells each component of `path`, by the one-name lookup.
pub(super) fn spelled(p: &Arc<dyn Provider>, path: &str) -> String {
    let mut out = Vec::new();
    let mut prefix = String::new();
    for comp in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(comp);
        let name = vfs_compose::stored_name(p.as_ref(), at(&prefix)).unwrap();
        out.push(name.unwrap_or_else(|| format!("<{comp}?>")));
    }
    out.join("/")
}

/// **The spelling of what the base has does not change when the game
/// writes under it.** The shim used to send folded paths, so the first
/// write created `data` in the write layer; the merged listing took the
/// upper's entry, name included, and `Data` became `data` for every
/// caller from then on. A directory's final path taken before the write
/// was no longer a prefix of a file's taken after it.
#[test]
fn a_write_under_a_base_directory_does_not_respell_it() {
    let (s, _d) = temp_storage();
    let (ov, upper) = game_overlay(&s);
    let fonts_before = spelled(&ov, "data/interface/fonts");
    assert_eq!(fonts_before, "Data/Interface/Fonts");

    // A write the way a folding client sends it: lower case throughout.
    ov.mkdir(at("data/skse")).unwrap();
    write_file(&ov, "data/skse/new log.txt", 0, b"log");
    write_file(&ov, "data/interface/fonts/b.ttf", 0, b"font2");
    // The upper now has its own, lower-case, rows for those directories.
    assert_eq!(listed(&upper, ""), ["data"]);

    assert_eq!(listed(&ov, ""), ["Data"], "the base's spelling stands");
    assert_eq!(listed(&ov, "Data"), ["Interface", "skse", "Skyrim.ini"]);
    assert_eq!(listed(&ov, "data/interface"), ["Fonts"]);
    let file_after = spelled(&ov, "data/interface/fonts/a.ttf");
    assert_eq!(file_after, "Data/Interface/Fonts/a.ttf");
    assert!(
        file_after.starts_with(&format!("{fonts_before}/")),
        "{fonts_before} (before the write) must still prefix {file_after} (after it)"
    );
    // The upper's entry is still the live one for everything but the name.
    write_file(&ov, "DATA/SKYRIM.INI", 0, b"[General]\nlonger now");
    let ini = ov.readdir(at("data")).unwrap();
    let ini = ini.iter().find(|e| e.name == "Skyrim.ini").unwrap();
    assert_eq!(ini.stat.size, 20, "the stat is the written copy's");
}

/// **A name the game creates is the name it gets back**, in a listing and
/// from the one-name lookup, however it is asked for afterwards — as on
/// NTFS. Reopening it in another case, for writing or with a
/// create-or-truncate, does not respell it either.
#[test]
fn a_created_name_keeps_the_spelling_it_was_created_with() {
    let (s, _d) = temp_storage();
    let (ov, _upper) = game_overlay(&s);
    const SAVE: &str =
        "Save12_ABCDEF01_0_4E6F726420486572_Tamriel_000123_20261002150000_1_1.ess";
    ov.mkdir(at("Saves")).unwrap();
    write_file(&ov, &format!("saves/{SAVE}"), 0, b"save");
    ov.mkdir(at("Data/SKSE")).unwrap();
    write_file(&ov, "DATA/skse/CommunityShaders.log", 0, b"log");

    assert_eq!(listed(&ov, ""), ["Data", "Saves"]);
    assert_eq!(listed(&ov, "SAVES"), [SAVE]);
    assert_eq!(listed(&ov, "data/skse"), ["CommunityShaders.log"]);
    assert_eq!(
        spelled(&ov, &format!("saves/{}", SAVE.to_lowercase())),
        format!("Saves/{SAVE}")
    );
    assert_eq!(
        spelled(&ov, "data/skse/communityshaders.log"),
        "Data/SKSE/CommunityShaders.log"
    );

    // Opened again in other cases: for append, and create-or-truncate.
    write_file(&ov, "data/SKSE/COMMUNITYSHADERS.LOG", 3, b"more");
    let (h, _, _) = ov
        .open(
            at("Data/Skse/communityshaders.LOG"),
            OPEN_WRITE | OPEN_CREATE | vfs_provider::OPEN_TRUNC,
        )
        .unwrap();
    ov.close(h).unwrap();
    assert_eq!(listed(&ov, "data/skse"), ["CommunityShaders.log"]);
}

/// A rename that changes only the letter case respells the entry and
/// nothing else: the bytes are there, nothing is hidden, and no whiteout
/// is left behind to hide the file under its own name.
#[test]
fn a_rename_to_another_case_respells_the_entry() {
    let (s, _d) = temp_storage();
    let (ov, upper) = game_overlay(&s);
    ov.mkdir(at("Saves")).unwrap();
    write_file(&ov, "Saves/Quick.ess", 0, b"save");

    ov.rename(at("saves/quick.ess"), at("saves/QUICK.ESS"))
        .unwrap();
    assert_eq!(listed(&ov, "saves"), ["QUICK.ESS"]);
    assert_eq!(read_file(&ov, "Saves/quick.ess"), b"save");
    assert_eq!(listed(&upper, "saves"), ["QUICK.ESS"], "and no whiteout");

    // A directory, with something in it.
    ov.rename(at("SAVES"), at("saves")).unwrap();
    assert_eq!(listed(&ov, ""), ["Data", "saves"]);
    assert_eq!(read_file(&ov, "Saves/Quick.ess"), b"save");

    // To a different name altogether: spelled as the destination was.
    ov.rename(at("saves/quick.ess"), at("saves/Slot One.ESS"))
        .unwrap();
    assert_eq!(listed(&ov, "saves"), ["Slot One.ESS"]);

    // Something only the base has keeps the base's spelling, like every
    // name the base has; the rename succeeds and hides nothing.
    ov.rename(at("Data/Skyrim.ini"), at("Data/SKYRIM.INI"))
        .unwrap();
    assert_eq!(listed(&ov, "data"), ["Interface", "Skyrim.ini"]);
    assert_eq!(read_file(&ov, "data/skyrim.ini"), b"[General]");
    assert!(upper.getattr(at("Data/.wh.Skyrim.ini")).unwrap().is_none());
}

/// The created spelling is in the catalog row, so it is there after the
/// storage is closed and opened again; and a row written before any of
/// this, whose name is the folded one, still reads as that.
#[test]
fn created_spellings_survive_a_reopen_and_folded_rows_still_read() {
    let d = vfs_testkit::tempdir().unwrap();
    {
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("write").unwrap();
        p.mkdir(at("Saves")).unwrap();
        write_file(&p, "Saves/Quick.ess", 0, b"save");
        // What every existing layer holds: names as the shim folded them.
        p.mkdir(at("data")).unwrap();
        write_file(&p, "data/old log.txt", 0, b"old");
        drop(p);
        s.sync().unwrap();
        s.close().unwrap();
    }
    let s = Storage::open(d.path(), cfg()).unwrap();
    let p = s.layer("write").unwrap();
    assert_eq!(listed(&p, ""), ["data", "Saves"]);
    assert_eq!(listed(&p, "saves"), ["Quick.ess"]);
    assert_eq!(listed(&p, "DATA"), ["old log.txt"]);
    assert_eq!(spelled(&p, "SAVES/QUICK.ESS"), "Saves/Quick.ess");
    assert_eq!(read_file(&p, "Data/Old Log.txt"), b"old");
}
