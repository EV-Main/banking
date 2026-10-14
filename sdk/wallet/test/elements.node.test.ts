// The wrappers mount the tag they know, so a registry entry is usable only
// when its name is there and points at that same tag.
import type { MfeEntry } from '@evinvest/mfe';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { fetchWalletRegistry, WALLET_MFE_PREFIX, WALLET_REGISTRY_URL, walletEntry } from '../src/index';

const WALLET: MfeEntry = {
  name: 'banking.wallet',
  tag: 'mfe-banking-wallet',
  kind: 'component',
  scriptUrl: '/wallet/mfe/wallet.js',
};

const PAY_INVOICE: MfeEntry = {
  name: 'banking.pay-invoice',
  tag: 'mfe-banking-pay-invoice',
  kind: 'component',
  scriptUrl: '/wallet/mfe/pay-invoice.js',
};

describe('walletEntry', () => {
  it('should_return_the_entry_whose_name_and_tag_match', () => {
    expect(walletEntry([WALLET, PAY_INVOICE], 'payInvoice')).toEqual({ ok: true, value: PAY_INVOICE });
  });

  it('should_refuse_when_the_registry_has_no_such_entry', () => {
    expect(walletEntry([PAY_INVOICE], 'wallet')).toEqual({ ok: false, error: 'registry has no banking.wallet' });
  });

  it('should_refuse_an_entry_whose_tag_differs_from_the_wrapper_tag', () => {
    const renamed: MfeEntry = { ...WALLET, tag: 'mfe-banking-wallet-next' };

    expect(walletEntry([renamed], 'wallet')).toEqual({
      ok: false,
      error: 'banking.wallet: tag mfe-banking-wallet-next, expected mfe-banking-wallet',
    });
  });
});

describe('registry location', () => {
  it('should_point_at_the_wallet_registry_on_evinvest_ltd', () => {
    expect(WALLET_REGISTRY_URL).toBe('https://evinvest.ltd/wallet/mfe/registry.json');
  });

  it('should_hold_bundles_to_the_wallet_mfe_directory', () => {
    expect(WALLET_MFE_PREFIX).toBe('https://evinvest.ltd/wallet/mfe/');
  });
});

describe('fetchWalletRegistry', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('should_resolve_bundles_inside_the_wallet_mfe_directory', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => Response.json([WALLET])));

    const result = await fetchWalletRegistry();

    expect(result).toEqual({ ok: true, value: [{ ...WALLET, scriptUrl: 'https://evinvest.ltd/wallet/mfe/wallet.js' }] });
  });

  it('should_refuse_a_registry_with_a_bundle_elsewhere_on_evinvest_ltd', async () => {
    // A wallet action page is on the same origin; it must never load as a script.
    vi.stubGlobal('fetch', vi.fn(async () => Response.json([{ ...WALLET, scriptUrl: '/wallet/pay' }])));

    const result = await fetchWalletRegistry();

    expect(result).toEqual({
      ok: false,
      error: `registry ${WALLET_REGISTRY_URL}: entry 0: banking.wallet: scriptUrl https://evinvest.ltd/wallet/pay is outside https://evinvest.ltd/wallet/mfe/`,
    });
  });

  it('should_refuse_redirects_and_pass_the_init_through', async () => {
    const fetchMock = vi.fn(async (_url: string | URL | Request, _init?: RequestInit) => Response.json([]));
    vi.stubGlobal('fetch', fetchMock);

    await fetchWalletRegistry(WALLET_REGISTRY_URL, { init: { cache: 'no-store' } });

    expect(fetchMock).toHaveBeenCalledWith(new URL(WALLET_REGISTRY_URL), { cache: 'no-store', redirect: 'error' });
  });

  it('should_hold_a_staging_registry_to_the_prefix_the_caller_names', async () => {
    const staging = 'https://stage.evinvest.ltd/wallet/mfe/';
    vi.stubGlobal('fetch', vi.fn(async () => Response.json([WALLET])));

    const result = await fetchWalletRegistry(`${staging}registry.json`, { allowedPrefix: staging });

    expect(result).toEqual({ ok: true, value: [{ ...WALLET, scriptUrl: `${staging}wallet.js` }] });
  });
});
