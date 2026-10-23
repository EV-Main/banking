// The frame-independent half of `AnimatedNumber`: decides *when* the final
// figure is written, so that it lands within a bounded time even if the count
// never gets an animation frame.
//
// Why this exists. The count is driven by `animateValue()` from motion, which advances
// on `requestAnimationFrame`. A hidden tab (a background window, a test harness
// with `document.visibilityState === "hidden"`) gets no frames, so the count never
// ticks past its starting figure and the tile reads "$0.00" for as long as the
// tab stays hidden — with the true value sitting in the API response the whole
// time (banking#346). Timers still fire in hidden tabs, throttled; frames do
// not. So the driver has three ways to reach the final figure and takes
// whichever comes first: the animation completing, the page going hidden, or a
// fallback timer set just past the animation's own duration.
//
// Everything with a side effect is injected so this can be tested in Node
// without a DOM or a frame loop.

export interface NumberRun {
  from: number;
  to: number;
  format: (n: number) => string;
  /** Put a string on screen. */
  write: (text: string) => void;
  /** The figure now on screen, so the next run can continue from it. */
  onShown: (n: number) => void;
}

export interface NumberDriverDeps {
  animate: (
    from: number,
    to: number,
    handlers: { onUpdate: (v: number) => void; onComplete: () => void },
  ) => { stop: () => void };
  isHidden: () => boolean;
  /** Called when the page goes hidden; returns the unsubscribe. */
  onHidden: (cb: () => void) => () => void;
  /** Returns the cancel. */
  setTimer: (cb: () => void, ms: number) => () => void;
  /** How long the fallback waits before writing the final figure. */
  fallbackMs: number;
}

/**
 * Counts `from` → `to`, guaranteeing the final figure is written. Returns a
 * `stop` for unmount or a new value arriving mid-count: it cancels everything
 * and leaves the screen where it is, so a restarted count continues from what
 * the eye last saw rather than jumping.
 */
export function driveNumber(run: NumberRun, deps: NumberDriverDeps): () => void {
  const { from, to, format, write, onShown } = run;
  if (from === to || deps.isHidden()) {
    write(format(to));
    onShown(to);
    return () => {};
  }
  write(format(from));
  let done = false;
  let unsubscribe = () => {};
  let cancelTimer = () => {};
  let stopAnimation = () => {};
  const finish = () => {
    if (done) return;
    done = true;
    unsubscribe();
    cancelTimer();
    stopAnimation();
  };
  const snap = () => {
    if (done) return;
    finish();
    write(format(to));
    onShown(to);
  };
  const controls = deps.animate(from, to, {
    onUpdate: (v) => {
      if (done) return;
      onShown(v);
      write(format(v));
    },
    onComplete: snap,
  });
  stopAnimation = () => controls.stop();
  // The animation may have completed synchronously (a zero-duration run).
  if (done) return () => {};
  unsubscribe = deps.onHidden(snap);
  cancelTimer = deps.setTimer(snap, deps.fallbackMs);
  return finish;
}
