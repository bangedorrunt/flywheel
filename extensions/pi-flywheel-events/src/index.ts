// First-party pi events bridge (bd-4n2bn, ADR-0039 D8 U11).
//
// Reads the bearer token from the FLYWHEEL_TORON_TOKEN_FILE 0600 file itself —
// no plaintext in process env, config, or journal — and re-reads it on 401.
// Subscribes to toron events/stream with an events/poll fallback and a
// persisted cursor; turns a decoded wake burst into one pi.sendMessage with
// deliverAs = steer (urgent) or followUp (normal). Carries only ids/names,
// never the mail body. Dedupes by bus event id against the doorbell's rung set.

import { getAgentDir, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import { coalesce, decodeFrame, type EventFrame } from "./decode.ts";
import { Deduper } from "./dedupe.ts";

const TOKEN_FILE = "FLYWHEEL_TORON_TOKEN_FILE";
const URL = process.env.FLYWHEEL_TORON_URL ?? "http://127.0.0.1:18080";
const MCP = URL.endsWith("/mcp") ? URL : `${URL.replace(/\/$/, "")}/mcp`;
const STATE = join(getAgentDir(), "flywheel-events.json");

interface State { subscriptionId?: string; cursor?: string; seen?: string[] }

async function loadState(): Promise<State> {
  try { return JSON.parse(await readFile(STATE, "utf8")); } catch { return {}; }
}
async function saveState(s: State): Promise<void> {
  await mkdir(dirname(STATE), { recursive: true });
  await writeFile(STATE, JSON.stringify(s), "utf8");
}

/** Token is read from the 0600 file on EVERY request; a refresh re-reads it. */
async function readToken(): Promise<string> {
  const path = process.env[TOKEN_FILE];
  if (!path) throw new Error(`${TOKEN_FILE} is not set`);
  const token = (await readFile(path, "utf8")).trim();
  if (!token) throw new Error(`${TOKEN_FILE} file is empty`);
  return token;
}

async function rpc(method: string, params: object, token: string, id = 1): Promise<any> {
  const res = await fetch(MCP, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      "MCP-Protocol-Version": "2026-07-28",
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
  });
  if (res.status === 401) throw new Error("401");
  const body = await res.text();
  const data = body.split("\n").find((l) => l.startsWith("data:"))?.slice(5).trim() ?? body;
  return JSON.parse(data);
}

async function subscribe(token: string): Promise<{ id: string; cursor: string }> {
  const r = await rpc("events/subscribe", {
    name: "mail.addressed", arguments: {}, delivery: "stream", ttl_us: 86_400_000_000,
  }, token);
  if (r.error) throw new Error(`events/subscribe: ${JSON.stringify(r.error)}`);
  return { id: String(r.result?.id ?? ""), cursor: String(r.result?.cursor ?? "0") };
}

/** One bounded stream read. Returns the frames and the fresh cursor, or null
 * when the stream failed (caller falls back to poll). */
async function streamOnce(
  sub: string, cursor: string, token: string,
  onFrame: (line: string) => void,
): Promise<{ raw: string[]; cursor: string } | null> {
  try {
    const res = await fetch(MCP, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: "text/event-stream",
        "MCP-Protocol-Version": "2026-07-28",
        authorization: `Bearer ${token}`,
        "last-event-id": cursor,
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "events/stream", params: { subscriptionId: sub, cursor } }),
    });
    if (res.status === 401) return null;
    if (!res.ok || !res.body) return null;
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    const raw: string[] = [];
    let fresh = cursor;
    let buffer = "";
    const deadline = Date.now() + 5_000;
    while (Date.now() < deadline) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, nl).trim();
        buffer = buffer.slice(nl + 1);
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        onFrame(data);
        raw.push(data);
        try {
          const v = JSON.parse(data);
          const c = v?.params?.cursor;
          if (c !== undefined && (v?.method ?? "").includes("gap")) fresh = String(c);
        } catch { /* non-json data line */ }
      }
      if (raw.length > 0) break;
    }
    await reader.cancel().catch(() => {});
    return raw.length > 0 ? { raw, cursor: fresh } : null;
  } catch {
    return null;
  }
}

async function pollOnce(sub: string, cursor: string, token: string): Promise<{ raw: string[]; cursor: string }> {
  const r = await rpc("events/poll", { subscriptionId: sub, cursor }, token);
  const items = r.result?.events ?? [];
  const raw = items.map((e: any) => JSON.stringify({
    method: "notifications/events/event",
    params: { _meta: { "io.modelcontextprotocol/subscriptionId": sub }, event: e },
  }));
  const fresh = String(r.result?.cursor ?? cursor);
  return { raw, cursor: fresh };
}

async function rungSet(): Promise<ReadonlySet<string>> {
  const path = process.env.FLYWHEEL_WAKE_RUNG_FILE;
  if (!path) return new Set();
  try {
    const rows = (await readFile(path, "utf8")).trim().split("\n").filter(Boolean);
    return new Set(rows.map((l) => { try { return String(JSON.parse(l).id ?? ""); } catch { return ""; } }));
  } catch { return new Set(); }
}

export default function flywheelEvents(pi: ExtensionAPI): void {
  pi.on("session_start", async () => {
    const deduper = new Deduper(512);
    let state = await loadState();
    if (!state.subscriptionId) {
      try {
        const sub = await subscribe(await readToken());
        state = { ...state, subscriptionId: sub.id, cursor: state.cursor ?? sub.cursor };
        await saveState(state);
      } catch { return; } // no token yet: the doorbell fallback stays in charge
    }
    const sub = state.subscriptionId!;
    let cursor = state.cursor ?? "0";
    // Background loop; pi owns the process lifetime.
    void (async () => {
      while (true) {
        try {
          let token = await readToken();
          let page: { raw: string[]; cursor: string } | null = null;
          try {
            page = await streamOnce(sub, cursor, token, () => {});
          } catch { /* fall through to poll */ }
          if (!page) {
            try {
              page = await pollOnce(sub, cursor, token);
            } catch (e) {
              if (String(e).includes("401")) token = await readToken(); // re-read on rotation
              continue;
            }
          }
          const frames: EventFrame[] = [];
          const rung = await rungSet();
          for (const data of page.raw) {
            const f = decodeFrame(data, sub);
            if (f && deduper.isNew(f.id) && !rung.has(f.id)) frames.push(f);
          }
          cursor = page.cursor;
          await saveState({ ...state, cursor, seen: deduper.snapshot() });
          const notice = coalesce(frames);
          if (notice) {
            pi.sendMessage(
              { customType: "flywheel-events", content: notice.hint, display: true,
                details: { event_id: notice.firstId, event_name: notice.name, count: notice.count } },
              { deliverAs: notice.deliverAs, triggerTurn: true },
            );
          }
          // Omit the cursor on the next stream POST unless it moved; the server
          // resumes from Last-Event-ID. Small backoff keeps an idle seat quiet.
          await new Promise((r) => setTimeout(r, 500));
        } catch {
          await new Promise((r) => setTimeout(r, 2_000));
        }
      }
    })();
  });
}
