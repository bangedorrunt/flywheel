# Staged-package build gate must skip workspace-excluded vendor paths (bd-g70r)

The toron commit-hook build gate (`build_gate_blocks` in
`toron-core/src/mailbox/guard.rs`) derived `cargo -p` specs from staged
paths by walking up to the nearest `[package]` manifest. A staged path
under `vendor/` — workspace-excluded per ADR-0040 §5, and home to crates
like `elm-effect-schema` that are in no manifest and not in `Cargo.lock`
— produced a `-p` spec cargo could never resolve, blocking the commit
that staged it (measured on bd-is18).

Fix (toron 9b8a4f79c7a8ecfc1f8e5bca08f4e5e6bc8614c3): `cargo_packages_for_staged`
now reads the root `[workspace] exclude` list and skips any staged path
under it — literal entries as dir prefixes, globs via the existing
`pattern_matches` rule. Pinned by
`package_walk_skips_workspace_excluded_vendor_paths` (red before, green
after). `check -p toron-core` green.

Note: `vendor/` itself has since been dropped (elm-effect 0.4.0 tracks the
sibling workspace), so the live failure mode is gone; the skip remains as
hardening for any future excluded dir. The `exclude = ["vendor"]` line in
the toron root manifest stays (ADR-0040 governs it).
