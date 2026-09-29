#!/bin/sh
# bd-4wkj: allocator_ext gate watch for HashMap::new_in.
#
# Red (exit 1) while the API sits behind #![feature(allocator_ext)]
# (rust issue #163177); green when it opens. Re-run on every nightly
# update — that event, plus a profiled hot map, is the only thing that
# reopens the bd-pk81 do-not-adopt decision.
#
# Uses the ambient rustc on purpose: the question is what the operator's
# current toolchain accepts, not what a pinned one does.
set -u
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM
cat > "$TMP/probe.rs" <<'EOF'
use std::alloc::Global;
use std::collections::HashMap;
fn main() {
    let mut m: HashMap<String, u64, Global> = HashMap::new_in(Global);
    m.insert("a".to_string(), 1);
}
EOF
if rustc --edition 2021 -O "$TMP/probe.rs" -o "$TMP/probe" 2>"$TMP/err.log"; then
    echo "OPEN: HashMap::new_in compiles on $(rustc --version)"
    exit 0
fi
if grep -q "allocator_ext" "$TMP/err.log"; then
    echo "GATED: HashMap::new_in still behind allocator_ext ($(rustc --version))"
    exit 1
fi
echo "UNKNOWN toolchain failure:"
cat "$TMP/err.log"
exit 2
