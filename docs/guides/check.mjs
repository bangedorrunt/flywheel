#!/usr/bin/env bun
// VERIFY fence for the guides (bd-z2d0). Catches broken MDX structure the eye skips.
// Walks docs/guides/** and checks every .mdx for frontmatter, balanced code fences,
// mermaid blocks with a known diagram header, stray body h1, and JSX components with
// no import surface.
//
// Extended (ADR-0004 D6 on toron.dev): a guide is evidence, not prose. Three more
// checks, all of which can fail a real page:
//
//   1. STRUCTURE  — the original set, unchanged.
//   2. COMPLETENESS — every ```bash block must be followed by a ```text block
//      showing what it printed, and the file must name at least one failure mode.
//      A command with no output is an unverified claim.
//   3. SURFACE — every command in a ```bash block must exist in the real CLI, with
//      only real flags. The surface is extracted by walking `<bin> --help`
//      recursively, so an invented flag fails here instead of shipping as truth.
//      This is the check that would have caught `br gate report --evidence` and
//      `toron workflow status`, both of which were written from memory and were
//      both wrong.
//
// Commands belonging to another tool are not this repo's to verify, so they are
// reported as skipped rather than checked. A missing own-binary is fail-closed:
// set GUIDES_SKIP_CLI=1 to bypass, which prints why.
//
// ponytail: mermaid is header-checked only; a full parse needs mermaid-cli, add when
// diagrams actually break in CI.
import { readdirSync, readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join, dirname, relative } from "node:path";
import { fileURLToPath } from "node:url";

// --- per-repo setting: the one line that differs between the four copies ---------
const OWN_BIN = "flywheel";
// ---------------------------------------------------------------------------------

const root = dirname(fileURLToPath(import.meta.url));

function walk(dir) {
  const out = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...walk(p));
    else if (e.name.endsWith(".mdx")) out.push(p);
  }
  return out;
}

const files = walk(root).sort();

const DIAGRAMS =
  /^(flowchart|graph|sequenceDiagram|stateDiagram-v2|stateDiagram|erDiagram|classDiagram|mindmap|timeline|gantt|pie|gitGraph|journey|quadrantChart|requirementDiagram|C4Context)\b/;

// A failure mode is a plain-text convention rather than a component, because the
// structure check above rejects JSX without an import surface and these guides
// are rendered as plain MDX.
const FAILS = /^\s*(?:>\s*)?(?:\*\*)?if it fails/im;

// ------------------------------------------------------------------ CLI surface

function help(bin, args) {
  const r = spawnSync(bin, [...args, "--help"], { encoding: "utf8" });
  return r.status === 0 ? (r.stdout || "") : null;
}

// Walk `<bin> --help` → `Commands:` block → recurse. Clap prints subcommands as
// two-space indented names, and flags one per line under `Options:`.
//
// Keys are the path WITHOUT the binary name (root is ""), so the caller must
// strip the leading token before descending. Getting that wrong is silent: the
// walk breaks at the first token and every command reports clean. The probe in
// the test suite exists because that bug shipped once.
function extractSurface(bin) {
  const commands = new Map(); // path (space-joined, no binary) → { flags, children }
  const queue = [[]];
  const seen = new Set();
  while (queue.length > 0) {
    const path = queue.shift();
    const key = path.join(" ");
    if (seen.has(key) || path.length > 3) continue;
    seen.add(key);
    const text = help(bin, path);
    if (text === null) continue;

    // long flag → does it take a value? (`--project <PROJECT>` does)
    const flags = new Map();
    const shorts = new Map();
    for (const line of text.split("\n")) {
      const m = line.match(/^\s+(?:-(\w),\s+)?--([a-z0-9][a-z0-9-]*)(.*)$/);
      if (!m) continue;
      const takesValue = /<[^>]+>/.test(m[3]);
      flags.set(m[2], takesValue);
      if (m[1]) shorts.set(m[1], takesValue);
    }

    const children = new Set();
    const lines = text.split("\n");
    const start = lines.findIndex((l) => /^Commands:\s*$/.test(l));
    if (start !== -1) {
      for (let i = start + 1; i < lines.length; i++) {
        if (/^\S/.test(lines[i])) break; // next top-level section
        // A subcommand line is two-space indented. The description is OPTIONAL:
        // `br comments` prints `  add   ` with nothing after it, and requiring a
        // description silently dropped the child, which turned a legal
        // `br comments add --message` into a reported unknown flag.
        const m = lines[i].match(/^\s{2}([a-z][a-z0-9-]*)(?:\s{2,}\S|\s*$)/);
        if (m && m[1] !== "help") children.add(m[1]);
      }
    }

    commands.set(key, { flags, shorts, children });
    for (const child of children) queue.push([...path, child]);
  }
  return commands;
}

// Split one shell line on already-joined continuations, honouring quotes, so a
// path with a space or a `#` inside a string does not confuse the tokenizer.
function tokenize(line) {
  const tokens = [];
  let cur = "";
  let quote = null;
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = null;
      else cur += ch;
      continue;
    }
    if (ch === '"' || ch === "'") { quote = ch; continue; }
    if (ch === " " || ch === "\t") {
      if (cur) { tokens.push(cur); cur = ""; }
      continue;
    }
    cur += ch;
  }
  if (cur) tokens.push(cur);
  return tokens;
}

// Join backslash continuations, then keep only the commands a reader would run:
// env assignments, `$` prompts, pipes, and redirections are all stripped away.
function commandLines(block) {
  const joined = block.replace(/\\\n\s*/g, " ");
  const out = [];
  for (const raw of joined.split("\n")) {
    let line = raw.trim();
    if (!line || line.startsWith("#") || line.startsWith("%%")) continue;
    line = line.replace(/^\$\s*/, "");
    line = line.split(/\|\||&&|\||;/)[0].trim();
    let tokens = tokenize(line);
    while (tokens.length > 0 && /^[A-Z_][A-Z0-9_]*=/.test(tokens[0])) tokens.shift();
    // Drop a trailing `# comment`. A `#` mid-token (inside a quoted value) is
    // not a comment and survives, because it never starts its own token.
    const hash = tokens.findIndex((t) => t.startsWith("#"));
    if (hash !== -1) tokens = tokens.slice(0, hash);
    // Drop synopsis placeholders (`[--check]`, `<thread-id>`, `<COMMAND>`). A
    // guide that puts usage syntax in a bash fence is writing a manual, not a
    // walkthrough, but it should not read as invented drift either.
    tokens = tokens.filter(
      (t) => !(t.startsWith("[") || t.endsWith("]") || (t.startsWith("<") && t.endsWith(">"))),
    );
    if (tokens.length === 0) continue;
    out.push({ text: line, tokens });
  }
  return out;
}

// Descend from the root, one bare token at a time. A bare token under a command
// that HAS subcommands must name one of them; under a leaf it is a positional
// argument and is allowed. Flags are checked against the node they apply to, and
// a value-taking flag consumes its value so `--project torondev` does not look
// like a subcommand named `torondev`.
function verifyCommand(tokens, surface) {
  const errors = [];
  const path = [];
  let node = surface.get("");
  if (!node) return errors;

  for (let i = 1; i < tokens.length; i++) {
    const t = tokens[i];
    if (t === "--") break;

    if (t.startsWith("-")) {
      const long = t.match(/^--([a-z0-9][a-z0-9-]*)/);
      const short = !long && t.match(/^-([A-Za-z])\b/);
      let takesValue = false;
      const where = `${OWN_BIN} ${path.join(" ")}`.trimEnd();
      if (long) {
        if (!node.flags.has(long[1])) {
          errors.push(`unknown flag --${long[1]} on \`${where}\``);
        } else {
          takesValue = node.flags.get(long[1]);
        }
      } else if (short) {
        if (!node.shorts.has(short[1])) {
          errors.push(`unknown flag -${short[1]} on \`${where}\``);
        } else {
          takesValue = node.shorts.get(short[1]);
        }
      }
      if (takesValue && !t.includes("=")) i++;
      continue;
    }

    if (node.children.size > 0) {
      if (node.children.has(t)) {
        path.push(t);
        node = surface.get(path.join(" "));
      } else {
        const where = `${OWN_BIN} ${path.join(" ")}`.trimEnd();
        errors.push(
          `unknown subcommand \`${t}\` under \`${where}\`` +
            ` (known: ${[...node.children].sort().join(", ")})`,
        );
      }
      continue;
    }
    // leaf: a positional argument, which is legal
  }
  return errors;
}

// --------------------------------------------------------------- block walking

function blocks(body) {
  const out = [];
  let cur = null;
  for (const [i, line] of body.split("\n").entries()) {
    if (line.startsWith("```")) {
      if (cur) { out.push(cur); cur = null; continue; }
      cur = { lang: line.slice(3).trim().split(/\s+/)[0], startLine: i + 1, lines: [] };
      continue;
    }
    if (cur) cur.lines.push(line);
  }
  if (cur) out.push({ ...cur, unterminated: true });
  return out;
}

function check(file, surface) {
  const errors = [];
  const warns = [];
  const src = readFileSync(file, "utf8");
  if (!src.startsWith("---\n")) return { errors: ["missing frontmatter"], warns };
  const close = src.indexOf("\n---\n", 4);
  if (close === -1) return { errors: ["unterminated frontmatter"], warns };
  const body = src.slice(close + 5);

  // --- 1. structure (unchanged behaviour) ---
  let fence = 0;
  let inMermaid = false;
  let mermaidChecked = false;
  for (const [i, line] of body.split("\n").entries()) {
    const n = i + 1;
    if (line.startsWith("```")) {
      if (fence === 0) {
        inMermaid = line.slice(3).trim() === "mermaid";
        mermaidChecked = false;
        fence = 1;
      } else {
        if (inMermaid && !mermaidChecked) errors.push(`${n}: mermaid block without a diagram header`);
        fence = 0;
        inMermaid = false;
      }
      continue;
    }
    if (fence === 1) {
      const t = line.trim();
      if (inMermaid && !mermaidChecked && t && !t.startsWith("%%")) {
        if (!DIAGRAMS.test(t)) errors.push(`${n}: unknown mermaid header: ${t}`);
        mermaidChecked = true;
      }
      continue;
    }
    if (/^#\s/.test(line)) errors.push(`${n}: body h1; titles live in frontmatter`);
    if (/<[A-Z][A-Za-z]*[\s/>]/.test(line)) errors.push(`${n}: JSX component without an import surface`);
  }
  if (fence !== 0) errors.push("unbalanced code fence");

  // --- 2. completeness ---
  const bs = blocks(body);
  const bashBlocks = bs.filter((b) => b.lang === "bash");
  for (const [idx, block] of bashBlocks.entries()) {
    const after = bs.filter((b) => b.startLine > block.startLine);
    const nextBash = bashBlocks[idx + 1];
    const between = after.filter((b) => !nextBash || b.startLine < nextBash.startLine);
    if (!between.some((b) => b.lang === "text")) {
      errors.push(`${block.startLine}: bash block with no \`\`\`text block after it — a command with no shown output is an unverified claim`);
    }
  }
  if (bashBlocks.length > 0 && !FAILS.test(body)) {
    errors.push("no failure mode; add an `If it fails:` paragraph to each step that can fail");
  }

  // --- 3. surface ---
  if (surface) {
    for (const block of bashBlocks) {
      for (const cmd of commandLines(block.lines.join("\n"))) {
        if (cmd.tokens[0] !== OWN_BIN) {
          warns.push(`${block.startLine}: skipped \`${cmd.tokens[0]}\` (not this repo's binary)`);
          continue;
        }
        for (const e of verifyCommand(cmd.tokens, surface)) {
          errors.push(`${block.startLine}: ${e} — in \`${cmd.text}\``);
        }
      }
    }
  }

  return { errors, warns };
}

// ------------------------------------------------------------------------ main

let failed = 0;
let surface = null;
if (process.env.GUIDES_SKIP_CLI === "1") {
  console.error(`SKIP surface check (GUIDES_SKIP_CLI=1): ${OWN_BIN} commands are NOT verified`);
} else {
  const probe = spawnSync(OWN_BIN, ["--help"], { encoding: "utf8" });
  if (probe.error || probe.status !== 0) {
    console.error(
      `FAIL cannot run \`${OWN_BIN} --help\`, so guide commands cannot be verified.\n` +
        `     install the binary, or set GUIDES_SKIP_CLI=1 and say why in the report.`,
    );
    process.exit(1);
  }
  surface = extractSurface(OWN_BIN);

  // Self-test: a gate that cannot fail is theater. This one shipped broken once
  // (the binary name was never stripped, so every command reported clean) and
  // the only reason it was caught is that a deliberate probe was run by hand.
  // Now the check refuses to run if it cannot reject an impossible command.
  const probeErrors = verifyCommand(
    [OWN_BIN, "__definitely-not-a-subcommand", "--__definitely-not-a-flag"],
    surface,
  );
  if (probeErrors.length === 0) {
    console.error(
      `FAIL the surface check is not biting: it accepted ${OWN_BIN} __definitely-not-a-subcommand.\n` +
        `     refuse to trust this run. Fix extractSurface/verifyCommand first.`,
    );
    process.exit(1);
  }
  console.log(`surface  ${OWN_BIN}: ${surface.size} command paths (self-test ok)`);
}

if (files.length === 0) {
  console.error("no .mdx guides found");
  failed = 1;
}
for (const f of files) {
  const rel = relative(root, f);
  const { errors, warns } = check(f, surface);
  for (const w of warns) console.error(`skip ${rel}: ${w}`);
  if (errors.length === 0) console.log(`ok   ${rel}`);
  else { failed = 1; for (const e of errors) console.error(`FAIL ${rel}: ${e}`); }
}
process.exit(failed);
