// Run fn at most once per ms, however often the returned trigger is called in between.
// The reader calls onChange for every relay event; rebuilding the whole snapshot each time
// stalled the main thread for seconds, until the relays' 20s history timeouts fired and the
// page never reached "Live". The page coalesces those calls into one rebuild.
type Timer = { set(fn: () => void, ms: number): unknown };
const realTimer: Timer = { set: (fn, ms) => setTimeout(fn, ms) };
export function coalesce(fn: () => void, ms: number, timer: Timer = realTimer): () => void {
  let pending = false;
  return () => {
    if (pending) return;
    pending = true;
    timer.set(() => { pending = false; fn(); }, ms);
  };
}
