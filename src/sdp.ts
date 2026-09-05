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
