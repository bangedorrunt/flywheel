grok was not captured (bd-0uzrq.3 gap).

Every frame this host produces for grok is its device-login gate ("Approve in
your browser to finish signing in … Waiting for approval"), not an input box, so
a capture would record an auth screen as if it were a prompt. Close the gap once
a signed-in grok pane exists: run `scripts/capture-pane-corpus.sh <out> grok`
and write the index next to it.

The `.txt` files in this directory are that login gate, kept for inspection and
NOT part of the corpus: the conformance test only reads directories that have an
`index.toml`.
