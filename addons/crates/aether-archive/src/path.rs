//! The one case-insensitive path comparison every archive lookup uses.

/// Lookup key for an archive or modlist path: `\` becomes `/` and letters are
/// lowercased (Unicode lowercase). Zip entries, BSA entries and resolved trees
/// all compare folded paths, so `Data\Textures\Ä.dds` and `data/textures/ä.DDS`
/// are the same file everywhere.
pub fn fold(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::fold;

    #[test]
    fn folds_separators_and_unicode_case() {
        assert_eq!(fold(r"Data\Meshes\A.NIF"), "data/meshes/a.nif");
        assert_eq!(fold("Ü/Straße.TXT"), "ü/straße.txt");
        assert_eq!(fold(r"a\b/c"), fold("A/B\\C"));
    }
}
