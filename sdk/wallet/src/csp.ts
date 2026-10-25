// What a host's Content-Security-Policy must allow for the wallet elements, as data. The
// host owns its policy; these helpers only list the wallet's sources and add them to it.
// No header parsing: a host keeps its policy as directives and serializes it once.
//
// Server-safe: nothing here touches the DOM.

import { WALLET_ORIGIN, walletMfePrefix, walletOriginOf } from './elements';
import type { WalletActionMode } from './inline';

/** A policy as directive name → sources, e.g. `{ 'script-src': ["'self'"] }`. */
export type CspDirectives = Readonly<Record<string, readonly string[]>>;

/** The directives the wallet needs. No `frame-src`: the wallet is never an iframe. */
export type WalletCsp = Readonly<Record<'script-src' | 'style-src' | 'connect-src', readonly string[]>>;

export interface WalletCspOptions {
  /** Default `'popup'`. `'inline'` adds the intent and step-up routes the in-page forms call. */
  mode?: WalletActionMode;
  /** Default {@link WALLET_ORIGIN}. Another value is for a staging stand only, and must be an exact `https:` origin. */
  origin?: string;
}

/**
 * The sources the wallet elements need, by directive, frozen.
 *
 * Every mode: the bundle directory for the script, its stylesheet and the registry; the
 * read routes the element calls on every connect — the shell's session, the wallet
 * balance, the CSRF route that also lists the kinds the BFF runs in the page, and the
 * history (fetched with a query, which CSP path matching ignores). Inline mode adds two
 * directories (a trailing `/` matches every path under it): the wallet API, for the
 * intent routes, and the shell's step-up routes. The element loads no fonts and no
 * images, so there is no `font-src` or `img-src`.
 *
 * Throws a `TypeError` for an `origin` that is not an exact `https:` origin (no path, no
 * trailing slash, no wildcard): that is a wrong config, not an outcome to handle.
 */
export function walletCsp({ mode = 'popup', origin = WALLET_ORIGIN }: WalletCspOptions = {}): WalletCsp {
  const base = walletOriginOf(origin);
  if (base === null) throw new TypeError(`walletCsp: origin must be an exact https: origin, got ${JSON.stringify(origin)}`);
  const prefix = walletMfePrefix(base);
  const connect = [
    prefix,
    `${base}/api/auth/session`,
    `${base}/cabinet/api/wallet`,
    `${base}/cabinet/api/wallet/csrf`,
    `${base}/cabinet/api/operations`,
  ];
  if (mode === 'inline') connect.push(`${base}/cabinet/api/wallet/`, `${base}/api/auth/stepup/`);
  return Object.freeze({
    'script-src': Object.freeze([prefix]),
    'style-src': Object.freeze([prefix]),
    'connect-src': Object.freeze(connect),
  });
}

// A space, `;` or `,` in a token would start another source, directive or policy.
const SOURCE = /^[^\s;,]+$/;
const DIRECTIVE = /^[a-z][a-z-]*$/;

function checkDirective(name: string, sources: readonly string[]): void {
  if (!DIRECTIVE.test(name)) throw new TypeError(`CSP: bad directive name ${JSON.stringify(name)}`);
  for (const source of sources) {
    if (!SOURCE.test(source)) throw new TypeError(`CSP: bad source ${JSON.stringify(source)} in ${name}`);
  }
}

/**
 * The policy as a header value: directives sorted by name and joined with `; `, sources in
 * their given order, deduplicated, joined with ` `. A directive with no sources is written
 * as its bare name, which a browser reads as `'none'`. Throws a `TypeError` for a name or
 * a source that would split into more than one token.
 */
export function serializeCsp(directives: CspDirectives): string {
  return Object.keys(directives)
    .sort()
    .map(name => {
      const sources = [...new Set(directives[name] ?? [])];
      checkDirective(name, sources);
      return [name, ...sources].join(' ');
    })
    .join('; ');
}

// A fetch directive the host leaves out falls back to `default-src`.
const FETCH_DIRECTIVES = new Set(['script-src', 'style-src', 'connect-src', 'img-src', 'font-src', 'frame-src', 'media-src', 'object-src', 'worker-src', 'manifest-src']);
// When the host sets these, they decide for elements instead of their parent directive.
const ELEMENT_DIRECTIVES: Readonly<Record<string, string>> = { 'script-src': 'script-src-elem', 'style-src': 'style-src-elem' };

function append(host: readonly string[], add: readonly string[]): readonly string[] {
  const merged = [...new Set([...host, ...add])];
  // `'none'` beside a source is ignored by browsers; dropped so the header says what it does.
  return merged.length > 1 ? merged.filter(source => source !== "'none'") : merged;
}

/**
 * The host's policy with the wallet's sources added, frozen. The host's sources and order
 * stay; a source already there is not repeated. Three rules keep the host's own resources
 * working:
 *
 * - a fetch directive the host leaves out starts from its `default-src`, since the new
 *   directive replaces that fallback;
 * - a directive the host restricts nowhere (neither it nor `default-src`) is left out:
 *   the host already allows everything there, and adding it would block the rest;
 * - `script-src-elem` and `style-src-elem`, when the host sets them, get the same sources
 *   as `script-src` and `style-src`, because they decide for `<script>` and `<link>`.
 */
export function mergeCsp(host: CspDirectives, wallet: CspDirectives): CspDirectives {
  const merged: Record<string, readonly string[]> = { ...host };
  for (const [name, sources] of Object.entries(wallet)) {
    const own = host[name] ?? (FETCH_DIRECTIVES.has(name) ? host['default-src'] : undefined);
    if (own !== undefined) merged[name] = append(own, sources);
    const elementName = ELEMENT_DIRECTIVES[name];
    const elementOwn = elementName === undefined ? undefined : host[elementName];
    if (elementName !== undefined && elementOwn !== undefined) merged[elementName] = append(elementOwn, sources);
  }
  for (const name of Object.keys(merged)) merged[name] = Object.freeze([...(merged[name] ?? [])]);
  return Object.freeze(merged);
}
