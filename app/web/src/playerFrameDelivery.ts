/** One decoded frame at a time. Ack only after the draw attempt, including failures. */
export function deliverPlayerFrame(draw: () => void, ack: (drawn: boolean) => void): void {
  // The source already paces frames and the backend holds only its newest frame.
  // Waiting for RAF here puts another display tick in the single-flight round trip
  // (or stops acks entirely when the WebView suspends animation callbacks).
  // Canvas updates are still composited by the browser at its display cadence.
  let drawn = false;
  try {
    draw();
    drawn = true;
  } finally {
    ack(drawn);
  }
}
