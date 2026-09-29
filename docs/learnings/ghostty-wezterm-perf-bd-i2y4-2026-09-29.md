# Ghostty/WezTerm perf lessons for flywheel (bd-i2y4)

Full report: /tmp/ghostty-wezterm-report.md (bead verify path).

## Reuse
- WezTerm PDU: varint len/ident/serial + version tag per variant (mixed-version skip) + COMPRESSED_MASK per frame. One PDU over UDS/TLS/SSH. Delta remote render. Predictive echo past RTT threshold (`local_echo_threshold_ms`).
- Ghostty: `apprt` split, `xev` loop, writer thread + SPSC `drainMailbox` batch, coalesce resize/scroll, duplicate hot bool to dodge lock, single contiguous mmap page-aligned block (zeroed), offset pointers for copy/serialize, u64 memset reinit, tiny-default + forced realloc for rare paths, per-row + full-page dirty flags, fail-fast on bad input.

## Apply to flywheel/doorbell/toron
1. PDU framing + version tag + compression flag for mailbox/doorbell IPC; UDS alongside HTTP 18980; delta-sync pane output; domains = projects.
2. Hot tick never blocks: batch-drain mailbox, coalesce wakes, cache hot flags tick-local (Buzz split stamps preserved).
3. Arena per tick, mmap page pool for big buffers, offset-based relocatable artifacts, dirty-set render only.
4. Measure throughput vs latency separately; keep ledger honesty (unknown stays unknown).

## Non-goals
No GPU/shaping/Lua; no new transport before PDU framing; no allocator crate without profiling proof.

Sources: ghostty Thread.zig/page.zig/PageList.zig, wezterm multiplexing docs + codec/src/lib.rs, terminal-benchmark directional numbers.
