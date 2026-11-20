// Bounded event-id dedupe, shared with the doorbell's rung set by id namespace
// (bd-4n2bn). Pure: a plain function over one id log, unit-testable with node.

export class Deduper {
  private seen: Set<string>;
  private order: string[];
  private cap: number;
  constructor(cap = 512) {
    this.cap = cap;
    this.seen = new Set();
    this.order = [];
  }

  isNew(id: string): boolean {
    if (this.seen.has(id)) return false;
    if (this.order.length >= this.cap) {
      const oldest = this.order.shift();
      if (oldest !== undefined) this.seen.delete(oldest);
    }
    this.seen.add(id);
    this.order.push(id);
    return true;
  }

  has(id: string): boolean {
    return this.seen.has(id);
  }

  snapshot(): string[] {
    return [...this.seen];
  }
}

/** Filter a burst to ids not already rung by the doorbell. Pure so one message
 * log can prove a bus event id delivered on both paths yields one hint. */
export function unpulled(frames: { id: string }[], rung: ReadonlySet<string>): string[] {
  return frames.filter((f) => !rung.has(f.id)).map((f) => f.id);
}
