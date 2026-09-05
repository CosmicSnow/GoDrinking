// Pure Sala (Stunar room) offer-routing helpers (no DOM, no Tauri).
//
// Failure A fix ("drain-then-drop"): the engine's poll_incoming_offers
// mem-take DRAINs the queue, so a frontend gate that returns early without
// requeueing destroys the offer. Every polled offer must end in exactly one
// of: answered, auto-watched-then-answered, or requeued/buffered —
// never silently dropped.
import { offerDedupeKey } from "./sdp";

export type SalaOffer = { from: string; sdp: string; offer_attempt: string };
export type SalaRosterEntry = { id: string; share?: boolean };

export type OfferGateDecision = "answer" | "auto-watch" | "requeue";

/**
 * Peek-don't-drop gate for one incoming offer. Outside Sala (Broadcast) or
 * for an already-watched peer the offer is answered. Inside Sala for an
 * unwatched peer: a member the roster shows as live (`share:true`) is
 * auto-watched and then answered; anything else goes back to the queue.
 */
export function decideSalaOfferGate(opts: {
  inSala: boolean;
  watching: ReadonlySet<string>;
  roster: readonly SalaRosterEntry[];
  from: string;
}): OfferGateDecision {
  if (!opts.inSala || opts.watching.has(opts.from)) return "answer";
  const member = opts.roster.find((entry) => entry.id === opts.from);
  if (member?.share === true) return "auto-watch";
  return "requeue";
}

/** Route a whole poll batch through the gate, preserving poll order. */
export function partitionSalaOffers(opts: {
  inSala: boolean;
  watching: ReadonlySet<string>;
  roster: readonly SalaRosterEntry[];
  offers: readonly SalaOffer[];
}): { answer: SalaOffer[]; autoWatch: SalaOffer[]; requeue: SalaOffer[] } {
  const answer: SalaOffer[] = [];
  const autoWatch: SalaOffer[] = [];
  const requeue: SalaOffer[] = [];
  for (const offer of opts.offers) {
    const decision = decideSalaOfferGate({
      inSala: opts.inSala,
      watching: opts.watching,
      roster: opts.roster,
      from: offer.from,
    });
    if (decision === "answer") answer.push(offer);
    else if (decision === "auto-watch") autoWatch.push(offer);
    else requeue.push(offer);
  }
  return { answer, autoWatch, requeue };
}

/**
 * Merge the local pending-offer buffer (held when the engine has no requeue
 * lane) ahead of a fresh poll. Pending first, deduped by from+attempt, so a
 * redelivered offer never double-answers and a held offer is never lost.
 */
export function nextSalaOfferBatch(
  pending: readonly SalaOffer[],
  polled: readonly SalaOffer[],
): SalaOffer[] {
  const seen = new Set<string>();
  const batch: SalaOffer[] = [];
  for (const offer of [...pending, ...polled]) {
    const key = offerDedupeKey(offer.from, offer.offer_attempt);
    if (seen.has(key)) continue;
    seen.add(key);
    batch.push(offer);
  }
  return batch;
}

/**
 * Watching-intent retention (Failure B companion): a transient share:false
 * flap must never delete intent. Auto-unwatch fires only for a member that
 * is gone from a KNOWN roster. Explicit Unwatch clicks bypass this helper
 * (they call unwatchMember directly).
 */
export function shouldAutoUnwatch(opts: {
  inSala: boolean;
  roster: readonly { id: string }[];
  selfId?: string | null;
  id: string;
}): boolean {
  if (!opts.inSala) return false;
  if (opts.roster.length === 0) return false; // roster not known yet: keep intent
  if (opts.selfId != null && opts.id === opts.selfId) return false;
  return !opts.roster.some((entry) => entry.id === opts.id);
}
