// An element built for the older host contract (apiVersion 2: no `actions` attribute,
// no `ev:wallet-result`, no mandate element) is refused, and the host's fallback stays.
// Its own file: the custom-element registry holds one class per tag, and the other
// React tests define the same tags at the current version.
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';

import { MandateButton, PayInvoiceButton, Wallet } from '../src/react/index';

const WALLET = 'mfe-banking-wallet';
const PAY = 'mfe-banking-pay-invoice';
const MANDATE = 'mfe-banking-mandate';

let container: HTMLDivElement;
let root: Root;

beforeAll(() => {
  for (const tag of [WALLET, PAY, MANDATE]) {
    customElements.define(tag, class extends HTMLElement {
      static apiVersion = 2;
    });
  }
});

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe('an apiVersion 2 element', () => {
  it('should_keep_the_wallet_fallback_and_mount_no_element', async () => {
    await act(async () => {
      root.render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" fallback={<p>Wallet unavailable</p>} />);
    });

    await expect.poll(() => container.querySelector('[data-mfe-state]')?.getAttribute('data-mfe-state')).toBe('failed');
    expect(container.textContent).toBe('Wallet unavailable');
    expect(container.querySelector(WALLET)).toBeNull();
  });

  it('should_keep_the_pay_invoice_fallback_and_mount_no_element', async () => {
    await act(async () => {
      root.render(<PayInvoiceButton scriptUrl="/wallet/mfe/pay.js" invoice="inv-1" onResult={() => {}} fallback={<p>Pay unavailable</p>} />);
    });

    await expect.poll(() => container.querySelector('[data-mfe-state]')?.getAttribute('data-mfe-state')).toBe('failed');
    expect(container.textContent).toBe('Pay unavailable');
    expect(container.querySelector(PAY)).toBeNull();
  });

  it('should_keep_the_mandate_fallback_and_mount_no_element', async () => {
    await act(async () => {
      root.render(<MandateButton scriptUrl="/wallet/mfe/mandate.js" request="req-1" onResult={() => {}} fallback={<p>Mandate unavailable</p>} />);
    });

    await expect.poll(() => container.querySelector('[data-mfe-state]')?.getAttribute('data-mfe-state')).toBe('failed');
    expect(container.textContent).toBe('Mandate unavailable');
    expect(container.querySelector(MANDATE)).toBeNull();
  });

  it('should_mount_the_element_when_the_host_lowers_minApiVersion_to_2', async () => {
    await act(async () => {
      root.render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" minApiVersion={2} fallback={<p>Wallet unavailable</p>} />);
    });

    await expect.poll(() => container.querySelector(WALLET)).not.toBeNull();
    expect(container.textContent).toBe('');
  });
});
