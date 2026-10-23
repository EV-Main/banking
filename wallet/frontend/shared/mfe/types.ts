// The microfrontend registry contract. Client-safe (no server imports) so both
// the BFF and client components can share these types.

export type MfeKind = "component" | "page";

export interface MfeEntry {
  /** Logical name, e.g. "risk.pnl-chart". */
  name: string;
  /** Globally-unique, versioned custom-element tag, e.g. "mfe-risk-pnl-chart". */
  tag: string;
  /** URL of the remote's self-registering ESM bundle (its own origin/CDN). */
  scriptUrl: string;
  /**
   * Subresource-Integrity hash for the bundle (e.g. "sha384-…"), delivered
   * atomically with `scriptUrl` so a swapped bundle fails to load. Required for
   * cross-origin remotes; optional for same-origin (relative) bundles served by
   * the cabinet itself, which the origin allow-list already constrains.
   */
  integrity?: string;
  /** Whether the remote is an inline widget or owns a whole route. */
  kind: MfeKind;
}
