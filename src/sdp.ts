// Pure SDP helpers for the viewer answer path (no DOM, no Tauri).
// A browser that cannot decode the offered video codec answers with the
// video m-section port set to 0 — sending that answer back only makes the
// host fail loudly while the viewer sits silent. Detect it locally first.

/** True when the SDP answer rejects the video stream (m=video port 0). */
export function videoSectionRejected(sdp: string): boolean {
  for (const raw of sdp.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line.startsWith("m=video ")) continue;
    return (line.split(/\s+/)[1] ?? "") === "0";
  }
  return false;
}

/** The rejected video m-line, if any (for diagnostics). */
export function rejectedVideoLine(sdp: string): string | null {
  for (const raw of sdp.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line.startsWith("m=video ")) continue;
    if ((line.split(/\s+/)[1] ?? "") === "0") return line;
  }
  return null;
}

// --- Phase-3B Stunar offer dedupe + opaque attempt echo -------------------
// Dedupe key is from + offer_attempt (never SDP contents). The answer echoes
// offer_attempt back opaquely so the Host can match the attempt without
// parsing SDP. Pure helpers so the contract is testable without DOM/Tauri.

/** Dedupe key for an incoming Stunar offer (sender + attempt only). */
export function offerDedupeKey(from: string, offerAttempt: string): string {
  return JSON.stringify([from, offerAttempt]);
}

/**
 * Once-per-attempt offer gate. Returns true when the offer should be
 * handled; records it in `seen` so redelivered polls stay silent.
 * A failed attempt must delete its key (allow retry on the next poll).
 */
export function shouldAcceptOffer(
  seen: Set<string>,
  from: string,
  offerAttempt: string,
): boolean {
  const key = offerDedupeKey(from, offerAttempt);
  if (seen.has(key)) return false;
  seen.add(key);
  return true;
}

/** Release a dedupe key so the next poll can retry the same attempt. */
export function releaseOfferKey(
  seen: Set<string>,
  from: string,
  offerAttempt: string,
): void {
  seen.delete(offerDedupeKey(from, offerAttempt));
}

/**
 * Build an answer envelope that echoes the offer attempt opaquely.
 * The attempt string is never interpreted — it is round-tripped verbatim.
 */
export function answerWithAttempt<T extends Record<string, unknown>>(
  answer: T,
  offerAttempt: string,
): T & { offer_attempt: string } {
  return { ...answer, offer_attempt: offerAttempt };
}

// --- Stunar answer identity -------------------------------------------------
// A Stunar answer envelope carries TWO different member ids and they must
// never be conflated:
//   - `id` is ALWAYS the answerer's own (watcher-side) member id, as the
//     host's viewers map / exact-offer-fence table knows it. The host looks
//     the answer up by this id; sending the sharer's id here is a
//     deterministic "no matching viewer" drop (silent black viewer).
//   - routing (`to`) is the SHARER the answer is addressed to.
// The watcher's member id lives engine-side in `stunar_viewer.member_id`
// (set by the ask/join handshake) and is exposed to the UI through the
// session snapshot as `viewer_member_id` (dual-role) or `self_id`
// (viewer branch, where both are the viewer id). Trace it — never guess.

/** Minimal snapshot shape needed to resolve the watcher's member id. */
export type AnswerIdentitySource = {
  viewer_member_id?: string | null;
  self_id?: string | null;
} | null | undefined;

/**
 * Resolve our own (watcher-side) member id for answer envelopes.
 * Dual-role processes prefer `viewer_member_id` so the host tile is never
 * conflated with self; pure viewers fall back to `self_id`. Null when the
 * snapshot has not arrived yet — callers must hold (not mislabel) the offer.
 */
export function resolveAnswerIdentity(session: AnswerIdentitySource): string | null {
  return session?.viewer_member_id ?? session?.self_id ?? null;
}

/**
 * Sala (Stunar room member-to-member) answer envelope: `id` is the
 * watcher's own member id, routed `to` the sharer by the caller.
 * `offer_attempt` is echoed opaquely (matched, never parsed).
 */
export function buildSalaAnswerEnvelope(
  selfId: string,
  sdp: string,
  offerAttempt: string,
): { type: "answer"; sdp: string; id: string; offer_attempt: string } {
  return answerWithAttempt({ type: "answer", sdp, id: selfId }, offerAttempt);
}

/**
 * Broadcast (LAN/Direct/Stunar join) answer envelope: `id` is the viewer id
 * the host minted the offer for (`offer.id`, echoed back so the host can
 * match the answer), or — for Stunar Broadcast, where `offer.id` names the
 * sharer side — our own resolved member id when known.
 */
export function buildBroadcastAnswerEnvelope(
  viewerId: string,
  sdp: string,
  offerAttempt: string,
): { type: "answer"; sdp: string; id: string; offer_attempt: string } {
  return answerWithAttempt({ type: "answer", sdp, id: viewerId }, offerAttempt);
}

// --- Per-member answer serialization ---------------------------------------
// Poll redeliveries and re-mints for the same `from` run concurrently (every
// await yields). A generation counter per member guarantees an older
// invocation never closes — or overwrites the store of — a newer PC: stale
// invocations clean up only their own PC and return before sending.

/** Generation guard for concurrent per-member answer invocations. Pure. */
export class AnswerGenerationGuard {
  private readonly generations = new Map<string, number>();

  /**
   * Begin an answer invocation for `from`. Returns its generation plus an
   * `isCurrent` check to run after every await: when false, this invocation
   * has been superseded — close only its own PC and return without
   * storing or answering.
   */
  begin(from: string): { generation: number; isCurrent: () => boolean } {
    const generation = (this.generations.get(from) ?? 0) + 1;
    this.generations.set(from, generation);
    return {
      generation,
      isCurrent: () => this.generations.get(from) === generation,
    };
  }

  /** Supersede any in-flight invocation (explicit Unwatch / teardown). */
  invalidate(from: string): void {
    this.generations.set(from, (this.generations.get(from) ?? 0) + 1);
  }

  /** Drop all generations (full Sala teardown). */
  reset(): void {
    this.generations.clear();
  }
}
