//! Shared integration-test source view.
//!
//! The cmd/ eviction moved handler bodies out of main.rs. A source contract
//! that used to grep main.rs must see the new home; reading only main.rs makes
//! a moved handler look deleted. This is the one reader for integration tests.

use std::path::PathBuf;

/// Every `.rs` under `src/`, sorted, concatenated with a newline separator.
pub fn crate_source() -> String {
    fn visit(dir: &std::path::Path, out: &mut String) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("src readable")
            .map(|e| e.expect("dir entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                visit(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push_str(&std::fs::read_to_string(&path).expect("read src"));
                out.push('\n');
            }
        }
    }
    let mut out = String::new();
    visit(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut out,
    );
    out
}
