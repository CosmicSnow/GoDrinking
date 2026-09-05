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
