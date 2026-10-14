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

export const WALLET_ELEMENTS = {
  wallet: { name: 'banking.wallet', tag: 'mfe-banking-wallet' },
  payInvoice: { name: 'banking.pay-invoice', tag: 'mfe-banking-pay-invoice' },
} as const;

export type WalletElement = keyof typeof WALLET_ELEMENTS;

export const WALLET_TAG = WALLET_ELEMENTS.wallet.tag;
export const PAY_INVOICE_TAG = WALLET_ELEMENTS.payInvoice.tag;

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
