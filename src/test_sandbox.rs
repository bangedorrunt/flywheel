//! Compatibility shim for test files that include this path with
//! `#[path = "../src/test_sandbox.rs"] mod test_sandbox;`.
//!
//! The sandbox lives in `crates/test-sandbox` (bd-jp9z1.4) so a `pub` item is
//! never dead in a test unit that uses only part of it. New tests should
//! `use flywheel_test_sandbox::TestSandbox;` instead of including this file.
pub use flywheel_test_sandbox::*;
