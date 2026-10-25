// On a scope host (Wallet with `scope`, PayInvoiceButton) a money action is
// taken out of the element's click only when its href is one of the known
// wallet actions on evinvest.ltd, and its result counts only when it echoes the
// state the popup was opened with; anything else is left to the element, and so is every action
// of a Wallet without `scope`, which runs on evinvest.ltd itself. One action per
// page is module state in the wallet core, so every fake popup is closed after
// each test. `ev:navigate` details are untrusted: only a root-relative path
// reaches the host's router. Tags are defined up front, so the loader resolves
// without a network; jsdom has no popups, so `window.open` returns a fake.
import { act, type ReactNode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';

import type { WalletActionOutcome } from '../src/index';
import { MandateButton, PayInvoiceButton, Wallet, type NavigateHandler } from '../src/react/index';

const WALLET = 'mfe-banking-wallet';
const PAY = 'mfe-banking-pay-invoice';
const MANDATE = 'mfe-banking-mandate';

interface FakePopup {
  closed: boolean;
  close: () => void;
  focus: () => void;
}

// The one cast in this file: jsdom cannot open a real `Window`; the helper
// reads only `closed`, `close()` and `focus()` and compares the object by identity.
function asWindow(popup: FakePopup): Window {
  return popup as unknown as Window;
}

let container: HTMLDivElement;
let root: Root;
let popups: FakePopup[] = [];

beforeAll(() => {
  customElements.define(WALLET, class extends HTMLElement {
    static apiVersion = 3;
  });
  customElements.define(PAY, class extends HTMLElement {
    static apiVersion = 3;
  });
  customElements.define(MANDATE, class extends HTMLElement {
    static apiVersion = 3;
  });
});

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  for (const popup of popups) popup.closed = true;
  popups = [];
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

async function render(node: ReactNode): Promise<void> {
  await act(async () => {
    root.render(node);
  });
}

function stubOpen() {
  const popup: FakePopup = {
    closed: false,
    close: () => {
      popup.closed = true;
    },
    focus: vi.fn(),
  };
  popups.push(popup);
  const open = vi.spyOn(window, 'open').mockReturnValue(asWindow(popup));
  return { popup, open };
}

/** The state the first popup was opened with, as the action page reads it from its URL. */
function stateOf(open: ReturnType<typeof stubOpen>['open']): string | null {
  const [href] = open.mock.calls[0] ?? [];
  return new URL(String(href)).searchParams.get('state');
}

function walletAction(target: Element | null, href: unknown): CustomEvent {
  const event = new CustomEvent('ev:wallet-action', { bubbles: true, composed: true, cancelable: true, detail: { href } });
  target?.dispatchEvent(event);
  return event;
}

function post(popup: FakePopup, origin: string, data: unknown): void {
  window.dispatchEvent(new MessageEvent('message', { origin, source: asWindow(popup), data }));
}

function navigate(target: Element | null, href: unknown): CustomEvent {
  const event = new CustomEvent('ev:navigate', { bubbles: true, composed: true, cancelable: true, detail: { href } });
  target?.dispatchEvent(event);
  return event;
}

describe('Wallet: ev:wallet-action', () => {
  it('should_take_a_wallet_action_into_a_popup_and_report_its_result', async () => {
    const { popup, open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" onResult={onResult} />);

    const event = walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/withdraw');
    window.dispatchEvent(
      new MessageEvent('message', {
        origin: 'https://evinvest.ltd',
        source: asWindow(popup),
        data: { type: 'ev:wallet-result', state: stateOf(open), outcome: 'done', reference: 'payout-1' },
      }),
    );

    expect(event.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => {
      expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
    });
  });

  it.each([
    ['another origin', 'https://evil.example/wallet/withdraw'],
    ['a path outside wallet', 'https://evinvest.ltd/admin/users'],
    ['a wallet path that is not an action', 'https://evinvest.ltd/wallet/transfer'],
    ['a dot-segment escape', 'https://evinvest.ltd/wallet/../admin'],
    ['a relative path', '/wallet/withdraw'],
    ['not a string', 42],
    ['an invoice id outside the allowed characters', 'https://evinvest.ltd/wallet/pay?invoice=a/../b'],
  ])('should_cancel_the_action_and_report_invalid_url_when_its_href_is_%s', async (_label, href) => {
    // Left to the element, the action would navigate this tab to the unchecked href.
    const { open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" onResult={onResult} />);

    const event = walletAction(container.querySelector(WALLET), href);

    expect(event.defaultPrevented).toBe(true);
    expect(open).not.toHaveBeenCalled();
    expect(onResult).toHaveBeenCalledWith({ status: 'invalid_url' });
  });

  it('should_refuse_a_production_href_when_a_staging_origin_is_set', async () => {
    const { open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" origin="https://stage.evinvest.ltd" onResult={onResult} />);

    const event = walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/withdraw');

    expect(event.defaultPrevented).toBe(true);
    expect(open).not.toHaveBeenCalled();
    expect(onResult).toHaveBeenCalledWith({ status: 'invalid_url' });
  });

  it('should_take_a_staging_action_and_its_staging_result_when_a_staging_origin_is_set', async () => {
    const { popup, open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" origin="https://stage.evinvest.ltd" onResult={onResult} />);

    walletAction(container.querySelector(WALLET), 'https://stage.evinvest.ltd/wallet/withdraw');
    window.dispatchEvent(
      new MessageEvent('message', {
        origin: 'https://stage.evinvest.ltd',
        source: asWindow(popup),
        data: { type: 'ev:wallet-result', state: stateOf(open), outcome: 'failed' },
      }),
    );

    expect(open).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => {
      expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { outcome: 'failed', reference: null } });
    });
  });

  it('should_ignore_a_result_that_does_not_echo_the_state', async () => {
    const { popup, open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" onResult={onResult} />);

    walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/withdraw');
    post(popup, 'https://evinvest.ltd', { type: 'ev:wallet-result', outcome: 'done', reference: 'forged' });
    post(popup, 'https://evinvest.ltd', { type: 'ev:wallet-result', state: stateOf(open), outcome: 'cancelled' });

    await vi.waitFor(() => {
      expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { outcome: 'cancelled', reference: null } });
    });
    expect(onResult).toHaveBeenCalledTimes(1);
  });

  it('should_navigate_to_the_rebuilt_action_with_this_page_as_return_to_when_the_popup_is_blocked', async () => {
    vi.spyOn(window, 'open').mockReturnValue(null);
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" onResult={onResult} />);
    // jsdom cannot navigate to another origin; stubbed after render, since the
    // loader resolves bundle URLs against the real location.
    const assign = vi.fn<(to: string) => void>();
    vi.stubGlobal('location', { href: 'https://sa.evinvest.ltd/billing?token=secret#row-7', origin: 'https://sa.evinvest.ltd', pathname: '/billing', assign });

    walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/pay?invoice=inv-1&amount=999');

    await vi.waitFor(() => {
      expect(assign).toHaveBeenCalledWith('https://evinvest.ltd/wallet/pay?invoice=inv-1&return_to=https%3A%2F%2Fsa.evinvest.ltd%2Fbilling');
    });
    expect(onResult).not.toHaveBeenCalled();
  });

  it('should_navigate_to_the_staging_action_when_the_popup_is_blocked_and_a_staging_origin_is_set', async () => {
    vi.spyOn(window, 'open').mockReturnValue(null);
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" origin="https://stage.evinvest.ltd" />);
    const assign = vi.fn<(to: string) => void>();
    vi.stubGlobal('location', { href: 'https://sa.evinvest.ltd/billing', origin: 'https://sa.evinvest.ltd', pathname: '/billing', assign });

    walletAction(container.querySelector(WALLET), 'https://stage.evinvest.ltd/wallet/pay?invoice=inv-1');

    await vi.waitFor(() => {
      expect(assign).toHaveBeenCalledWith('https://stage.evinvest.ltd/wallet/pay?invoice=inv-1&return_to=https%3A%2F%2Fsa.evinvest.ltd%2Fbilling');
    });
  });

  it('should_leave_every_action_to_the_element_without_scope', async () => {
    const { open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" onResult={onResult} />);

    const event = walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/withdraw');

    expect(event.defaultPrevented).toBe(false);
    expect(open).not.toHaveBeenCalled();
    expect(onResult).not.toHaveBeenCalled();
  });

  it('should_start_taking_actions_once_scope_is_set_on_the_mounted_wallet', async () => {
    const { open } = stubOpen();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" />);
    const mounted = container.querySelector(WALLET);

    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" />);
    const event = walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/withdraw');

    expect(container.querySelector(WALLET)).toBe(mounted);
    expect(event.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledTimes(1);
  });

  it('should_set_the_scope_attribute', async () => {
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" />);

    expect(container.querySelector(WALLET)?.getAttribute('scope')).toBe('sa');
  });
});

describe('Wallet: ev:navigate', () => {
  it('should_hand_a_valid_path_to_onNavigate_and_cancel_the_click_when_taken', async () => {
    const onNavigate = vi.fn<NavigateHandler>(() => true);
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" onNavigate={onNavigate} />);

    const event = navigate(container.querySelector(WALLET), '/wallet/history?page=2');

    expect(onNavigate).toHaveBeenCalledWith('/wallet/history?page=2');
    expect(event.defaultPrevented).toBe(true);
  });

  it.each([
    ['protocol-relative', '//evil.example'],
    ['backslash', '/\\evil.example'],
    ['absolute', 'https://evil.example/'],
    ['not a string', null],
  ])('should_not_call_onNavigate_for_a_%s_href', async (_label, href) => {
    const onNavigate = vi.fn<NavigateHandler>(() => true);
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" onNavigate={onNavigate} />);

    const event = navigate(container.querySelector(WALLET), href);

    expect(onNavigate).not.toHaveBeenCalled();
    expect(event.defaultPrevented).toBe(false);
  });
});

describe('PayInvoiceButton', () => {
  it('should_set_the_invoice_and_scope_attributes', async () => {
    await render(<PayInvoiceButton scriptUrl="/wallet/mfe/pay.js" invoice="inv-1" scope="sa" onResult={() => {}} />);

    const button = container.querySelector(PAY);
    expect(button?.getAttribute('invoice')).toBe('inv-1');
    expect(button?.getAttribute('scope')).toBe('sa');
  });

  it('should_take_a_wallet_action_into_a_popup', async () => {
    const { popup, open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<PayInvoiceButton scriptUrl="/wallet/mfe/pay.js" invoice="inv-1" onResult={onResult} />);

    const event = walletAction(container.querySelector(PAY), 'https://evinvest.ltd/wallet/pay?invoice=inv-1');
    window.dispatchEvent(
      new MessageEvent('message', {
        origin: 'https://evinvest.ltd',
        source: asWindow(popup),
        data: { type: 'ev:wallet-result', state: stateOf(open), outcome: 'cancelled' },
      }),
    );

    expect(event.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => {
      expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { outcome: 'cancelled', reference: null } });
    });
  });

  it('should_report_one_result_for_a_double_click', async () => {
    const { popup, open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<PayInvoiceButton scriptUrl="/wallet/mfe/pay.js" invoice="inv-1" onResult={onResult} />);
    const button = container.querySelector(PAY);

    const firstClick = walletAction(button, 'https://evinvest.ltd/wallet/pay?invoice=inv-1');
    const secondClick = walletAction(button, 'https://evinvest.ltd/wallet/pay?invoice=inv-1');
    post(popup, 'https://evinvest.ltd', { type: 'ev:wallet-result', state: stateOf(open), outcome: 'done', reference: 'payment-1' });

    await vi.waitFor(() => {
      expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { outcome: 'done', reference: 'payment-1' } });
    });
    expect(onResult).toHaveBeenCalledTimes(1);
    expect(open).toHaveBeenCalledTimes(1);
    expect(popup.focus).toHaveBeenCalledTimes(1);
    expect(firstClick.defaultPrevented).toBe(true);
    expect(secondClick.defaultPrevented).toBe(true);
  });

  it('should_cancel_an_action_and_report_invalid_url_when_its_href_is_not_a_wallet_action', async () => {
    const { open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<PayInvoiceButton scriptUrl="/wallet/mfe/pay.js" invoice="inv-1" onResult={onResult} />);

    const event = walletAction(container.querySelector(PAY), 'https://evil.example/wallet/pay');

    expect(event.defaultPrevented).toBe(true);
    expect(open).not.toHaveBeenCalled();
    expect(onResult).toHaveBeenCalledWith({ status: 'invalid_url' });
  });
});

function walletResult(target: Element | null, detail: unknown): CustomEvent {
  const event = new CustomEvent('ev:wallet-result', { bubbles: true, composed: true, detail });
  target?.dispatchEvent(event);
  return event;
}

describe('Wallet: inline mode', () => {
  it('should_set_the_actions_attribute_to_inline', async () => {
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" mode="inline" />);

    expect(container.querySelector(WALLET)?.getAttribute('actions')).toBe('inline');
  });

  it('should_not_set_the_actions_attribute_in_popup_mode', async () => {
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" />);

    expect(container.querySelector(WALLET)?.hasAttribute('actions')).toBe(false);
  });

  it('should_report_an_in_page_result_with_its_kind', async () => {
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" mode="inline" onResult={onResult} />);

    walletResult(container.querySelector(WALLET), { kind: 'withdraw', outcome: 'done', reference: 'payout-7' });

    expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { kind: 'withdraw', outcome: 'done', reference: 'payout-7' } });
  });

  it.each([
    ['an unknown kind', { kind: 'transfer', outcome: 'done' }],
    ['an unknown outcome', { kind: 'move', outcome: 'ok' }],
    ['no detail', null],
  ])('should_ignore_an_in_page_result_with_%s', async (_label, detail) => {
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" mode="inline" onResult={onResult} />);

    walletResult(container.querySelector(WALLET), detail);

    expect(onResult).not.toHaveBeenCalled();
  });

  it('should_ignore_an_in_page_result_in_popup_mode', async () => {
    // A popup host never opted into in-page actions, so the event is not its result.
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" onResult={onResult} />);

    walletResult(container.querySelector(WALLET), { kind: 'withdraw', outcome: 'done' });

    expect(onResult).not.toHaveBeenCalled();
  });

  it('should_still_take_an_offered_action_into_a_popup_in_inline_mode', async () => {
    // A kind the BFF does not allow for this host still comes as an offer.
    const { open } = stubOpen();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" mode="inline" />);

    const event = walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/withdraw');

    expect(event.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledTimes(1);
  });

  it('should_switch_the_mounted_wallet_to_inline_when_the_mode_changes', async () => {
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" onResult={onResult} />);
    const mounted = container.querySelector(WALLET);

    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" mode="inline" onResult={onResult} />);
    walletResult(container.querySelector(WALLET), { kind: 'move', outcome: 'cancelled' });

    expect(container.querySelector(WALLET)).toBe(mounted);
    expect(mounted?.getAttribute('actions')).toBe('inline');
    expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { kind: 'move', outcome: 'cancelled', reference: null } });
  });

  it('should_remove_the_actions_attribute_and_stop_reporting_when_the_mode_returns_to_popup', async () => {
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" mode="inline" onResult={onResult} />);

    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" scope="sa" mode="popup" onResult={onResult} />);
    walletResult(container.querySelector(WALLET), { kind: 'move', outcome: 'done' });

    expect(container.querySelector(WALLET)?.hasAttribute('actions')).toBe(false);
    expect(onResult).not.toHaveBeenCalled();
  });

  it('should_report_an_in_page_result_and_leave_offered_actions_to_the_element_without_scope', async () => {
    // Accepted behaviour: on evinvest.ltd itself an offered action runs in place, but
    // the in-page result still reaches the host.
    const { open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<Wallet scriptUrl="/wallet/mfe/wallet.js" mode="inline" onResult={onResult} />);

    const action = walletAction(container.querySelector(WALLET), 'https://evinvest.ltd/wallet/withdraw');
    walletResult(container.querySelector(WALLET), { kind: 'withdraw', outcome: 'failed' });

    expect(action.defaultPrevented).toBe(false);
    expect(open).not.toHaveBeenCalled();
    expect(onResult).toHaveBeenCalledTimes(1);
    expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { kind: 'withdraw', outcome: 'failed', reference: null } });
  });
});

describe('PayInvoiceButton: inline mode', () => {
  it('should_set_the_actions_attribute_and_report_an_in_page_payment', async () => {
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<PayInvoiceButton scriptUrl="/wallet/mfe/pay.js" invoice="inv-1" mode="inline" onResult={onResult} />);
    const button = container.querySelector(PAY);

    walletResult(button, { kind: 'pay', outcome: 'done', reference: 'payment-9' });

    expect(button?.getAttribute('actions')).toBe('inline');
    expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { kind: 'pay', outcome: 'done', reference: 'payment-9' } });
  });
});

describe('MandateButton', () => {
  it('should_set_the_request_and_scope_attributes_without_actions_by_default', async () => {
    await render(<MandateButton scriptUrl="/wallet/mfe/mandate.js" request="req-1" scope="sa" onResult={() => {}} />);

    const button = container.querySelector(MANDATE);
    expect(button?.getAttribute('request')).toBe('req-1');
    expect(button?.getAttribute('scope')).toBe('sa');
    expect(button?.hasAttribute('actions')).toBe(false);
  });

  it('should_take_a_mandate_action_into_a_popup_and_report_its_result', async () => {
    const { popup, open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<MandateButton scriptUrl="/wallet/mfe/mandate.js" request="req-1" onResult={onResult} />);

    const event = walletAction(container.querySelector(MANDATE), 'https://evinvest.ltd/wallet/mandate?request=req-1');
    post(popup, 'https://evinvest.ltd', { type: 'ev:wallet-result', state: stateOf(open), outcome: 'done' });

    expect(event.defaultPrevented).toBe(true);
    expect(open).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => {
      expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { outcome: 'done', reference: null } });
    });
  });

  it('should_cancel_an_action_and_report_invalid_url_when_its_href_is_not_a_wallet_action', async () => {
    const { open } = stubOpen();
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<MandateButton scriptUrl="/wallet/mfe/mandate.js" request="req-1" onResult={onResult} />);

    const event = walletAction(container.querySelector(MANDATE), 'https://evil.example/wallet/mandate?request=req-1');

    expect(event.defaultPrevented).toBe(true);
    expect(open).not.toHaveBeenCalled();
    expect(onResult).toHaveBeenCalledWith({ status: 'invalid_url' });
  });

  it('should_set_the_actions_attribute_and_report_an_in_page_grant_in_inline_mode', async () => {
    const onResult = vi.fn<(outcome: WalletActionOutcome) => void>();
    await render(<MandateButton scriptUrl="/wallet/mfe/mandate.js" request="req-1" mode="inline" onResult={onResult} />);
    const button = container.querySelector(MANDATE);

    walletResult(button, { kind: 'mandate', outcome: 'done', reference: 'mandate-3' });

    expect(button?.getAttribute('actions')).toBe('inline');
    expect(onResult).toHaveBeenCalledWith({ status: 'result', result: { kind: 'mandate', outcome: 'done', reference: 'mandate-3' } });
  });
});
