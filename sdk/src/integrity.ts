// Integrity of the wallet bundle a host loads. Banking publishes the sha256 of each shipped
// file as one line (`npm run --silent mfe:pins -- wallet` in wallet/frontend):
//
//   wallet.js=<64 lowercase hex>,wallet.css=<64 lowercase hex>
//
// A host keeps that line in its config and uses it two ways: in the browser, as an import
// map `integrity` entry for wallet.js (the @evinvest/mfe loader injects
// `<script type="module" src>` without an `integrity` attribute, and an import map is the
// one place a host can add it); at startup or in CI, by fetching both files and comparing.
//
// Needs `crypto.subtle` (Node >= 20, browsers in a secure context). Server-safe.

import type { ParseResult } from '@evinvest/mfe';

import { WALLET_MFE_PREFIX, WALLET_ORIGIN, walletMfePrefix, walletOriginOf } from './elements';

/** The files a pin line may name, relative to the bundle directory. */
export const WALLET_FILES = ['wallet.js', 'wallet.css'] as const;
export type WalletFile = (typeof WALLET_FILES)[number];

/** Published sha256 per file, 64 lowercase hex each. */
export type WalletPins = Readonly<Record<WalletFile, string>>;

export type WalletSriAlgorithm = 'sha256' | 'sha384' | 'sha512';

const DIGEST: Readonly<Record<WalletSriAlgorithm, string>> = { sha256: 'SHA-256', sha384: 'SHA-384', sha512: 'SHA-512' };
const HEX_PIN = /^[0-9a-f]{64}$/;
const SRI = /^sha(?:256|384|512)-[A-Za-z0-9+/]+={0,2}$/;

function isWalletFile(name: string): name is WalletFile {
  return WALLET_FILES.some(file => file === name);
}

function base64(bytes: Uint8Array): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
}

async function digest(alg: WalletSriAlgorithm, bytes: BufferSource): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest(DIGEST[alg], bytes));
}

/**
 * The Subresource Integrity value of `bytes`, e.g. `'sha384-…'`. Rejects only when the
 * runtime has no `crypto.subtle` (a browser page over plain http).
 */
export async function walletSri(bytes: BufferSource, alg: WalletSriAlgorithm = 'sha384'): Promise<string> {
  return `${alg}-${base64(await digest(alg, bytes))}`;
}

/**
 * A published pin as an SRI value, `'sha256-<base64>'`, without downloading the file; null
 * when `pin` is not 64 lowercase hex. sha256 is a valid SRI algorithm, so the result goes
 * straight into {@link walletImportMap}.
 */
export function pinToSri(pin: string): string | null {
  if (!HEX_PIN.test(pin)) return null;
  const bytes = new Uint8Array(32);
  for (let i = 0; i < 32; i++) bytes[i] = Number.parseInt(pin.slice(i * 2, i * 2 + 2), 16);
  return `sha256-${base64(bytes)}`;
}

/**
 * The pin line banking publishes, checked: one line of `file=hex` pairs joined by `,`
 * (surrounding whitespace and the final newline are fine). Refused: a file other than
 * {@link WALLET_FILES}, a hash that is not 64 lowercase hex, a file named twice, a file
 * missing — a host that pins only the script would load an unpinned stylesheet.
 */
export function parseWalletPins(text: string): ParseResult<WalletPins> {
  const line = text.trim();
  if (line === '') return { ok: false, error: 'pins: empty' };
  if (/[\r\n]/.test(line)) return { ok: false, error: 'pins: more than one line' };
  const found = new Map<WalletFile, string>();
  for (const pair of line.split(',')) {
    const parts = pair.split('=');
    const [name, pin] = parts;
    if (parts.length !== 2 || name === undefined || pin === undefined) return { ok: false, error: `pins: ${JSON.stringify(pair)} is not file=hex` };
    if (!isWalletFile(name)) return { ok: false, error: `pins: unknown file ${JSON.stringify(name)}` };
    if (!HEX_PIN.test(pin)) return { ok: false, error: `pins: ${name} is not 64 lowercase hex` };
    if (found.has(name)) return { ok: false, error: `pins: ${name} named twice` };
    found.set(name, pin);
  }
  const js = found.get('wallet.js');
  const css = found.get('wallet.css');
  if (js === undefined) return { ok: false, error: 'pins: wallet.js missing' };
  if (css === undefined) return { ok: false, error: 'pins: wallet.css missing' };
  return { ok: true, value: Object.freeze({ 'wallet.js': js, 'wallet.css': css }) };
}

/** One file of {@link verifyWalletBundle}: `match`, or why not. */
export type WalletFileCheck =
  | { file: string; url: string; status: 'match' }
  /** The bytes differ from the pin; `actual` is their sha256. */
  | { file: string; url: string; status: 'mismatch'; actual: string }
  /** Not checked: no fetch, a network error, a redirect, an HTTP error. */
  | { file: string; url: string; status: 'unavailable'; error: string }
  /** Not fetched: an unknown file name, a bad pin, a URL outside the bundle directory, a bad `origin`. */
  | { file: string; url: string; status: 'refused'; error: string };

export interface WalletBundleCheck {
  /** True only when every pinned file matched. */
  ok: boolean;
  files: readonly WalletFileCheck[];
}

export interface VerifyWalletBundleOptions {
  /** Default `globalThis.fetch`. */
  fetch?: typeof fetch;
  /** Default {@link WALLET_ORIGIN}; another value is for a staging stand only, an exact `https:` origin. */
  origin?: string;
}

async function checkFile(fetchImpl: typeof fetch, file: string, url: string, pin: string): Promise<WalletFileCheck> {
  let body: ArrayBuffer;
  try {
    // `redirect: 'error'`: a file that moved is not the file that was pinned. `no-store`:
    // the check reads what the server ships now, not an older cached copy.
    const res = await fetchImpl(url, { redirect: 'error', cache: 'no-store', credentials: 'omit' });
    if (res.url !== '' && res.url !== url) return { file, url, status: 'unavailable', error: `redirected to ${res.url}` };
    if (!res.ok) return { file, url, status: 'unavailable', error: `HTTP ${res.status}` };
    body = await res.arrayBuffer();
  } catch (cause) {
    return { file, url, status: 'unavailable', error: cause instanceof Error ? cause.message : String(cause) };
  }
  let actual: string;
  try {
    actual = hex(await digest('sha256', body));
  } catch (cause) {
    return { file, url, status: 'unavailable', error: cause instanceof Error ? cause.message : String(cause) };
  }
  return actual === pin ? { file, url, status: 'match' } : { file, url, status: 'mismatch', actual };
}

/**
 * Fetch each pinned file from the bundle directory and compare its sha256 with the pin.
 * For a host's startup or CI: it says whether the files served now are the ones published.
 * It does not protect a browser that fetches later — that is the import map's job. Never
 * rejects; every failure is a {@link WalletFileCheck}. The keys of `pins` are read as
 * untrusted, so a plain-JS caller's unknown name is `refused`, not fetched.
 */
export async function verifyWalletBundle(pins: WalletPins, options: VerifyWalletBundleOptions = {}): Promise<WalletBundleCheck> {
  const origin = walletOriginOf(options.origin ?? WALLET_ORIGIN);
  const prefix = origin === null ? null : walletMfePrefix(origin);
  const fetchImpl = options.fetch ?? (typeof fetch === 'function' ? fetch : undefined);
  const files = await Promise.all(
    Object.entries(pins).map(([file, pin]): Promise<WalletFileCheck> | WalletFileCheck => {
      if (prefix === null) return { file, url: '', status: 'refused', error: 'origin must be an exact https: origin' };
      if (!isWalletFile(file)) return { file, url: '', status: 'refused', error: 'unknown file' };
      const url = new URL(file, prefix).href;
      if (!url.startsWith(prefix)) return { file, url, status: 'refused', error: 'outside the bundle directory' };
      if (typeof pin !== 'string' || !HEX_PIN.test(pin)) return { file, url, status: 'refused', error: 'pin is not 64 lowercase hex' };
      if (fetchImpl === undefined) return { file, url, status: 'unavailable', error: 'no fetch' };
      return checkFile(fetchImpl, file, url, pin);
    }),
  );
  return { ok: files.length > 0 && files.every(check => check.status === 'match'), files };
}

/** The `<script type="importmap">` content that pins one module script. */
export interface WalletImportMap {
  integrity: Readonly<Record<string, string>>;
}

/**
 * An import map that gives `scriptUrl` (default the wallet bundle) the integrity `sri`. A
 * host renders `JSON.stringify(map)` as `<script type="importmap">` before any module
 * script. Then a browser that supports import-map integrity refuses a wallet.js whose
 * bytes differ; a browser that does not ignores the key and loads it unchecked. Throws a
 * `TypeError` for an `sri` that is not one SRI value or a `scriptUrl` that is not an
 * absolute `https:` URL: import-map keys resolve against the page, so a relative one would
 * pin the wrong URL.
 */
export function walletImportMap(sri: string, scriptUrl: string = `${WALLET_MFE_PREFIX}wallet.js`): WalletImportMap {
  if (!SRI.test(sri)) throw new TypeError(`walletImportMap: ${JSON.stringify(sri)} is not an SRI value`);
  let url: URL;
  try {
    url = new URL(scriptUrl);
  } catch (cause) {
    throw new TypeError(`walletImportMap: ${JSON.stringify(scriptUrl)} is not an absolute URL`, { cause });
  }
  if (url.protocol !== 'https:') throw new TypeError(`walletImportMap: ${JSON.stringify(scriptUrl)} is not https`);
  return Object.freeze({ integrity: Object.freeze({ [url.href]: sri }) });
}
