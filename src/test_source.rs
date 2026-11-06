//! Test-only crate-source reader (bd-wave12-heal-suite-3r4q3).
//!
//! After the cmd/ eviction, moved-code tests must search the whole crate,
//! not `main.rs`. This module is the ONLY source-discovery path for those
//! tests: [`crate_source`] concatenates every `.rs` file under `src/` in
//! deterministic (sorted) order, so a grep for a handler finds it wherever
//! it moved. An "is NOT in main" assertion should check `main.rs` explicitly
//! (via `include_str!`) and the new home positively — not rely on absence
//! from a partial source.

use std::path::PathBuf;

/// Every `.rs` file under `src/`, in deterministic (sorted) order, as paths
/// relative to the crate root (e.g. `src/cmd/spawn.rs`).
pub fn crate_source_files() -> Vec<PathBuf> {
    fn visit(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("test_source: src dir must be readable")
            .map(|e| e.expect("test_source: dir entry must be readable").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                visit(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    visit(&root, &mut out);
    out
}

/// Concatenated contents of every `.rs` file under `src/`, in the deterministic
/// order of [`crate_source_files`], joined by a single newline. Pure content —
/// no file markers — so occurrence counts match what the code actually holds.
pub fn crate_source() -> String {
    let mut acc = String::new();
    for path in crate_source_files() {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("test_source: cannot read {}: {e}", path.display()));
        acc.push_str(&text);
        acc.push('\n');
    }
    acc
}

/// Byte offset of the first `needle` occurrence; panics naming the needle when
/// absent (fail-loud: a renamed handler must update its tripwire, never pass
/// on a vacuous search).
pub fn find_close(src: &str, needle: &str) -> usize {
    src.find(needle)
        .unwrap_or_else(|| panic!("test_source: needle not found: {needle}"))
}

/// `len` bytes starting at the first `needle`, clamped to the end of `src`.
/// Marker-based slicing: no hard-coded absolute offsets.
pub fn slice_from<'a>(src: &'a str, needle: &str, len: usize) -> &'a str {
    let start = find_close(src, needle);
    let end = (start + len).min(src.len());
    &src[start..end]
}
