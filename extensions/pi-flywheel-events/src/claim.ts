// One atomic claim per bus event id (bd-tdpix). O_CREAT|O_EXCL across
// processes and languages: Rust uses OpenOptions.create_new(true), node uses
// fs.openSync(path, "wx"). Exactly one claimer rings; the rest stay silent.

import { closeSync, mkdirSync, openSync, readdirSync, statSync, unlinkSync } from "node:fs";
import { join } from "node:path";

export function markerPath(dir: string, eventId: string): string {
  const safe = [...eventId]
    .map((c) => (/[A-Za-z0-9_-]/.test(c) ? c : "_"))
    .slice(0, 120)
    .join("");
  return join(dir, `${safe || "empty"}.rung`);
}

/** True only for the FIRST claimer of this id. */
export function claimEvent(dir: string, eventId: string): boolean {
  mkdirSync(dir, { recursive: true });
  try {
    const fd = openSync(markerPath(dir, eventId), "wx");
    closeSync(fd);
    return true;
  } catch (e: any) {
    if (e?.code === "EEXIST") return false;
    throw e;
  }
}

/** Remove markers older than maxAgeSecs; returns how many. Best effort. */
export function sweepRung(dir: string, maxAgeSecs: number, nowSecs: number): number {
  let removed = 0;
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return 0;
  }
  for (const name of names) {
    try {
      const path = join(dir, name);
      const mtimeSecs = Math.floor(statSync(path).mtimeMs / 1000);
      if (nowSecs - mtimeSecs > maxAgeSecs && (unlinkSync(path), true)) removed += 1;
    } catch {
      // a marker another process just removed is fine
    }
  }
  return removed;
}
