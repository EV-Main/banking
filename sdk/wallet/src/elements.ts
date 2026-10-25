// Where the banking wallet microfrontends live and what they are called. Tags
// follow the Rust `mfe!` scheme, `mfe-{service}-{name}`.

import { entryFor, fetchRegistry, type MfeEntry, type ParseResult } from '@evinvest/mfe';

/** The one origin that may carry out a money action and report its result. */
export const WALLET_ORIGIN = 'https://evinvest.ltd';

/**
 * The directory every wallet bundle is served from. `fetchWalletRegistry`
 * refuses a registry with a bundle outside it, and a host's CSP names the same
 * path in `script-src`.
 */
export const WALLET_MFE_PREFIX = `${WALLET_ORIGIN}/wallet/mfe/`;

/** The published registry of wallet bundles; relative `scriptUrl`s resolve against it. */
export const WALLET_REGISTRY_URL = `${WALLET_MFE_PREFIX}registry.json`;

/**
 * The host-contract version the wallet elements speak, set on each element class as
 * `static apiVersion`. Our own number, not `@evinvest/mfe`'s `API_VERSION`: the wallet
 * contract grew (the `actions` attribute, the `ev:wallet-result` event, the mandate
 * element) while the generic one did not. The React wrappers pass it as the default
 * `minApiVersion`, so an older element is refused and the host shows its fallback.
 */
export const WALLET_API_VERSION = 3;

export const WALLET_ELEMENTS = {
  wallet: { name: 'banking.wallet', tag: 'mfe-banking-wallet' },
  payInvoice: { name: 'banking.pay-invoice', tag: 'mfe-banking-pay-invoice' },
  mandate: { name: 'banking.mandate', tag: 'mfe-banking-mandate' },
} as const;

export type WalletElement = keyof typeof WALLET_ELEMENTS;

export const WALLET_TAG = WALLET_ELEMENTS.wallet.tag;
export const PAY_INVOICE_TAG = WALLET_ELEMENTS.payInvoice.tag;
export const MANDATE_TAG = WALLET_ELEMENTS.mandate.tag;

// Hostnames, an optional port, nothing else: the value lands in a CSP header and a URL
// prefix, so a `*`, a path or a separator must not get through.
const HTTPS_ORIGIN = /^https:\/\/[a-z0-9.-]+(?::[0-9]{1,5})?$/;

/**
 * `origin` when it is an exact `https:` origin as `URL` spells it (lowercase, no
 * trailing slash, no path, no credentials), else null. For the `origin` options that
 * point the helpers at a staging stand.
 */
export function walletOriginOf(origin: string): string | null {
  if (!HTTPS_ORIGIN.test(origin)) return null;
  try {
    return new URL(origin).origin === origin ? origin : null;
  } catch {
    return null;
  }
}

/** The bundle directory on `origin`: {@link WALLET_MFE_PREFIX} for the default origin. */
export function walletMfePrefix(origin: string): string {
  return `${origin}/wallet/mfe/`;
}

export interface WalletRegistryOptions {
  /** Default {@link WALLET_MFE_PREFIX}; another value is for a staging stand only. */
  allowedPrefix?: string;
  init?: RequestInit;
}

/**
 * Fetch the wallet registry (default {@link WALLET_REGISTRY_URL}), with every
 * bundle held to {@link WALLET_MFE_PREFIX} and redirects refused (as in
 * `fetchRegistry`). Failures are results.
 */
export function fetchWalletRegistry(
  url: string | URL = WALLET_REGISTRY_URL,
  { allowedPrefix = WALLET_MFE_PREFIX, init }: WalletRegistryOptions = {},
): Promise<ParseResult<MfeEntry[]>> {
  return fetchRegistry(url, init === undefined ? { allowedPrefix } : { allowedPrefix, init });
}

/** The registry entry for `element`, refused when missing or naming another tag (`entryFor`). */
export function walletEntry(entries: readonly MfeEntry[], element: WalletElement): ParseResult<MfeEntry> {
  return entryFor(entries, WALLET_ELEMENTS[element]);
}
