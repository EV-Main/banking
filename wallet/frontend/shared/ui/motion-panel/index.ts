// Public API of the panel half of the cabinet's motion. Import from
// `@/shared/ui/motion-panel`.
//
// Why this is not in `@/shared/ui/motion` with the other primitives. `Panel`
// animates its own size (`layout="size"`), so it is built on `motion.div` and
// carries the library's full feature set — layout projection and drag included.
// The rest of the motion slice runs on `m` and the much smaller `domAnimation`
// (see ../motion/features). The bundler does not prune unused re-exports of an
// app module, so as long as `Panel` sat behind that barrel every page that
// imported a `StaggerItem` paid for projection too. Behind its own entry point
// the full set reaches only the screens that actually open a panel.
//
// Curves and durations still come from `@/shared/ui/motion`'s tokens.
export {
  Panel,
  PanelPresence,
  PanelSwap,
  type PanelProps,
  type PanelSwapProps,
} from "./panel";
