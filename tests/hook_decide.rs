// bd-9os20: the Claude module's pure decision lives in src/hook_decide.rs but
// is not yet wired into the binary (the operator-run installer owns that CLI
// boundary). Include it here so its tests are part of the normal proof.
#[path = "../src/hook_decide.rs"]
mod hook_decide;
