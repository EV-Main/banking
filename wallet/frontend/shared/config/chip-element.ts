// The account chip element's names, shared by both sides of it: the chip itself
// (features/account-chip) and the wallet host as one of its hosts (application/chip-navigation).
// Down here rather than in the chip's slice because the slice's index is also the chip
// bundle's entry, and a host reaching for two strings must not pull the chip UI into its own
// client bundle with them.
//
// Import-free so `node --test` runs its importers as they are.

/** The custom element every chip renders in; its events are dispatched from it. */
export const CHIP_TAG = "mfe-cabinet-account-chip";

/**
 * Cancelable, bubbling, composed: a plain same-origin click on a chip menu link, offered to
 * the host's router. `preventDefault()` takes it (the chip cancels the click); left alone,
 * the browser follows the link.
 */
export const NAVIGATE_EVENT = "ev:navigate";

/** `detail` of NAVIGATE_EVENT: the destination as path + query + hash, same-origin. */
export type NavigateDetail = { href: string };
