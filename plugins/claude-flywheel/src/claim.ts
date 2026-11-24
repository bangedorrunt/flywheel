// bd-tdpix wake-rung, restricted-loader safe: delegate O_CREAT|O_EXCL to
// `flywheel __claim-wake` via the host spawn primitive so zero `node:*` imports
// are required inside Claude Code's plugin loader.

export interface ClaimPiece {
  stream: string;
  text: string;
}

export interface ClaimChild {
  stdout: AsyncIterable<ClaimPiece>;
}

export type ClaimSpawnFn = (
  argv: readonly string[],
  env: Record<string, string | undefined>,
) => ClaimChild | AsyncIterable<ClaimPiece>;

const asClaimChild = (c: ClaimChild | AsyncIterable<ClaimPiece>): ClaimChild =>
  Symbol.asyncIterator in c ? { stdout: c as AsyncIterable<ClaimPiece> } : c;

export function claimArgv(
  dir: string,
  eventId: string,
  flywheelBin = "flywheel",
): readonly string[] {
  return [flywheelBin, "__claim-wake", "--dir", dir, "--event-id", eventId];
}

export async function claimEvent(
  spawn: ClaimSpawnFn,
  dir: string,
  eventId: string,
  flywheelBin = "flywheel",
): Promise<boolean> {
  const child = asClaimChild(spawn(claimArgv(dir, eventId, flywheelBin), {}));
  let out = "";
  for await (const piece of child.stdout) {
    if (piece.stream === "stdout") out += piece.text;
  }
  return out.trim() === "claimed";
}
