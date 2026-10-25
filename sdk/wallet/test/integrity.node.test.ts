// A host pins the published wallet bundle two ways: an import-map integrity entry in the
// browser, and a fetch-and-compare check at startup or in CI. Digest literals below were
// computed with openssl over the same bytes, not with the code under test:
//   printf 'alert(1)' | openssl dgst -sha384 -binary | openssl base64 -A
//   printf 'alert(1)' | openssl dgst -sha256
//   printf 'body{}'   | openssl dgst -sha256
import { describe, expect, it, vi } from 'vitest';

import { parseWalletPins, pinToSri, verifyWalletBundle, walletImportMap, walletSri, type WalletPins } from '../src/index';

const JS = 'alert(1)';
const CSS = 'body{}';
const JS_SHA256 = '6e11c72f7cf6bc383152dd16ddd5903aba6bb1c99d6b6639a4bb0b838185fa92';
const CSS_SHA256 = '7c98040a541657584690ae2a1cc3b42a8b53b159cc60c5d3abbfecbaeac6c94a';
const PINS: WalletPins = { 'wallet.js': JS_SHA256, 'wallet.css': CSS_SHA256 };
const JS_URL = 'https://evinvest.ltd/wallet/mfe/wallet.js';
const CSS_URL = 'https://evinvest.ltd/wallet/mfe/wallet.css';

// Copied into a fresh buffer: `encode` is typed as possibly shared, which `BufferSource` refuses.
function bytes(text: string): Uint8Array<ArrayBuffer> {
  return new Uint8Array(new TextEncoder().encode(text));
}

/** A fetch that serves `files` by URL and 404s anything else. */
function serve(files: Record<string, string>) {
  return vi.fn(async (input: string | URL | Request, _init?: RequestInit) => {
    const body = files[String(input)];
    return body === undefined ? new Response('missing', { status: 404 }) : new Response(body);
  });
}

describe('walletSri', () => {
  it('should_default_to_sha384', async () => {
    expect(await walletSri(bytes(JS))).toBe('sha384-HT2E9NfWiuQ/w1PRai+hTyqW16NIoCGA/m8VQDUopfAtcz6YQjtsMmQd5uRbVDpW');
  });

  it('should_use_sha256_when_asked', async () => {
    expect(await walletSri(bytes(JS), 'sha256')).toBe('sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI=');
  });

  it('should_use_sha512_when_asked', async () => {
    expect(await walletSri(bytes(JS), 'sha512')).toBe(
      'sha512-+uuYUxxe7oWIShQrWEmMn/fixz/rxDP4qcAZddXLDM3nN8/tpk1ZC2jXQk6N+mXE65jwfzNVUJL/qjA3y9KbuQ==',
    );
  });
});

describe('pinToSri', () => {
  it('should_turn_a_hex_pin_into_the_sha256_sri_of_the_same_bytes', () => {
    expect(pinToSri(JS_SHA256)).toBe('sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI=');
  });

  it.each([
    ['uppercase hex', JS_SHA256.toUpperCase()],
    ['63 characters', JS_SHA256.slice(1)],
    ['65 characters', `${JS_SHA256}0`],
    ['a non-hex character', `g${JS_SHA256.slice(1)}`],
    ['an empty string', ''],
  ])('should_return_null_for_%s', (_label, pin) => {
    expect(pinToSri(pin)).toBeNull();
  });
});

describe('parseWalletPins', () => {
  it('should_read_the_published_line_with_its_final_newline', () => {
    expect(parseWalletPins(`wallet.js=${JS_SHA256},wallet.css=${CSS_SHA256}\n`)).toEqual({ ok: true, value: PINS });
  });

  it('should_read_the_files_in_any_order', () => {
    expect(parseWalletPins(`wallet.css=${CSS_SHA256},wallet.js=${JS_SHA256}`)).toEqual({ ok: true, value: PINS });
  });

  it.each([
    ['an empty text', '  \n', 'pins: empty'],
    ['two lines', `wallet.js=${JS_SHA256}\nwallet.css=${CSS_SHA256}`, 'pins: more than one line'],
    ['a pair with two equals signs', 'a=b=c', 'pins: "a=b=c" is not file=hex'],
    ['a pair without an equals sign', `wallet.js${JS_SHA256}`, `pins: "wallet.js${JS_SHA256}" is not file=hex`],
    ['an unknown file', `wallet.js=${JS_SHA256},x.js=${CSS_SHA256}`, 'pins: unknown file "x.js"'],
    ['uppercase hex', `wallet.js=${JS_SHA256.toUpperCase()},wallet.css=${CSS_SHA256}`, 'pins: wallet.js is not 64 lowercase hex'],
    ['a file named twice', `wallet.js=${JS_SHA256},wallet.js=${JS_SHA256}`, 'pins: wallet.js named twice'],
    ['the stylesheet missing', `wallet.js=${JS_SHA256}`, 'pins: wallet.css missing'],
    ['the script missing', `wallet.css=${CSS_SHA256}`, 'pins: wallet.js missing'],
  ])('should_refuse_%s', (_label, text, error) => {
    expect(parseWalletPins(text)).toEqual({ ok: false, error });
  });
});

describe('verifyWalletBundle', () => {
  it('should_match_both_files_and_fetch_without_redirects_cache_or_credentials', async () => {
    const fetchMock = serve({ [JS_URL]: JS, [CSS_URL]: CSS });

    const check = await verifyWalletBundle(PINS, { fetch: fetchMock });

    expect(check).toEqual({
      ok: true,
      files: [
        { file: 'wallet.js', url: JS_URL, status: 'match' },
        { file: 'wallet.css', url: CSS_URL, status: 'match' },
      ],
    });
    expect(fetchMock).toHaveBeenCalledWith(JS_URL, { redirect: 'error', cache: 'no-store', credentials: 'omit' });
    expect(fetchMock).toHaveBeenCalledWith(CSS_URL, { redirect: 'error', cache: 'no-store', credentials: 'omit' });
  });

  it('should_report_a_mismatch_with_the_actual_sha256', async () => {
    const check = await verifyWalletBundle(PINS, { fetch: serve({ [JS_URL]: CSS, [CSS_URL]: CSS }) });

    expect(check.ok).toBe(false);
    expect(check.files[0]).toEqual({ file: 'wallet.js', url: JS_URL, status: 'mismatch', actual: CSS_SHA256 });
  });

  it('should_report_an_http_error_as_unavailable', async () => {
    const check = await verifyWalletBundle(PINS, { fetch: serve({ [CSS_URL]: CSS }) });

    expect(check.ok).toBe(false);
    expect(check.files[0]).toEqual({ file: 'wallet.js', url: JS_URL, status: 'unavailable', error: 'HTTP 404' });
  });

  it('should_report_a_rejected_fetch_as_unavailable_with_its_message', async () => {
    const fetchMock = vi.fn(async () => {
      throw new TypeError('fetch failed');
    });

    const check = await verifyWalletBundle(PINS, { fetch: fetchMock });

    expect(check.ok).toBe(false);
    expect(check.files).toEqual([
      { file: 'wallet.js', url: JS_URL, status: 'unavailable', error: 'fetch failed' },
      { file: 'wallet.css', url: CSS_URL, status: 'unavailable', error: 'fetch failed' },
    ]);
  });

  it('should_report_a_response_from_another_url_as_unavailable', async () => {
    const moved = new Response(JS);
    Object.defineProperty(moved, 'url', { value: 'https://evinvest.ltd/elsewhere.js' });
    const fetchMock = vi.fn(async () => moved);

    const check = await verifyWalletBundle({ 'wallet.js': JS_SHA256 } as WalletPins, { fetch: fetchMock });

    expect(check.files[0]).toEqual({ file: 'wallet.js', url: JS_URL, status: 'unavailable', error: 'redirected to https://evinvest.ltd/elsewhere.js' });
  });

  it('should_refuse_an_unknown_file_without_fetching_it', async () => {
    // Keys are read as untrusted: a plain-JS caller can pass anything.
    const fetchMock = serve({ [JS_URL]: JS });
    const pins = { 'wallet.js': JS_SHA256, '../x.js': CSS_SHA256 } as unknown as WalletPins;

    const check = await verifyWalletBundle(pins, { fetch: fetchMock });

    expect(check.ok).toBe(false);
    expect(check.files[1]).toEqual({ file: '../x.js', url: '', status: 'refused', error: 'unknown file' });
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('should_refuse_a_bad_pin_without_fetching_it', async () => {
    const fetchMock = serve({ [JS_URL]: JS, [CSS_URL]: CSS });

    const check = await verifyWalletBundle({ 'wallet.js': JS_SHA256, 'wallet.css': 'abc' }, { fetch: fetchMock });

    expect(check.files[1]).toEqual({ file: 'wallet.css', url: CSS_URL, status: 'refused', error: 'pin is not 64 lowercase hex' });
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('should_refuse_every_file_for_a_bad_origin', async () => {
    const fetchMock = serve({});

    const check = await verifyWalletBundle(PINS, { fetch: fetchMock, origin: 'http://evinvest.ltd' });

    expect(check.files).toEqual([
      { file: 'wallet.js', url: '', status: 'refused', error: 'origin must be an exact https: origin' },
      { file: 'wallet.css', url: '', status: 'refused', error: 'origin must be an exact https: origin' },
    ]);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('should_fetch_from_a_staging_origin', async () => {
    const fetchMock = serve({ 'https://x.y:8443/wallet/mfe/wallet.js': JS, 'https://x.y:8443/wallet/mfe/wallet.css': CSS });

    const check = await verifyWalletBundle(PINS, { fetch: fetchMock, origin: 'https://x.y:8443' });

    expect(check.ok).toBe(true);
  });

  it('should_not_be_ok_for_empty_pins', async () => {
    const check = await verifyWalletBundle({} as WalletPins, { fetch: serve({}) });

    expect(check).toEqual({ ok: false, files: [] });
  });
});

describe('walletImportMap', () => {
  it('should_pin_the_wallet_bundle_by_default', () => {
    expect(walletImportMap('sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI=')).toEqual({
      integrity: { [JS_URL]: 'sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI=' },
    });
  });

  it('should_pin_another_absolute_https_url', () => {
    const map = walletImportMap('sha384-HT2E9NfWiuQ/w1PRai+hTyqW16NIoCGA/m8VQDUopfAtcz6YQjtsMmQd5uRbVDpW', 'https://x.y:8443/wallet/mfe/wallet.js');

    expect(map.integrity).toEqual({ 'https://x.y:8443/wallet/mfe/wallet.js': 'sha384-HT2E9NfWiuQ/w1PRai+hTyqW16NIoCGA/m8VQDUopfAtcz6YQjtsMmQd5uRbVDpW' });
  });

  it.each([
    ['a hex pin', JS_SHA256],
    ['two SRI values', 'sha256-a= sha256-b='],
    ['md5', 'md5-abc='],
  ])('should_throw_for_an_sri_that_is_%s', (_label, sri) => {
    expect(() => walletImportMap(sri)).toThrow(TypeError);
  });

  it.each([
    ['relative', '/wallet/mfe/wallet.js'],
    ['http', 'http://evinvest.ltd/wallet/mfe/wallet.js'],
  ])('should_throw_for_a_script_url_that_is_%s', (_label, scriptUrl) => {
    expect(() => walletImportMap('sha256-bhHHL3z2vDgxUt0W3dWQOrprscmda2Y5pLsLg4GF+pI=', scriptUrl)).toThrow(TypeError);
  });
});
