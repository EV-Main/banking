// openWalletAction opens evinvest.ltd in a popup and waits for its result.
// Only a message from that popup, from the wallet origin, echoing the state the
// popup was opened with, counts; every other
// way out (closed, timeout, abort) is a value, and each one closes the popup.
// One action per page: while a popup is in flight, another call focuses it and
// resolves `busy`. That in-flight popup is module state, so each test imports a
// fresh copy of the module. jsdom has no popups and no `Window.closed`, so
// `window.open` returns a fake that only carries what the helper reads.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { walletNavigationHref, walletPaymentUrl, type WalletActionOutcome } from '../src/index';

type WalletModule = typeof import('../src/index');

const ACTION = 'https://evinvest.ltd/wallet/pay?invoice=inv-1';
const RESULT = { type: 'ev:wallet-result', outcome: 'done', reference: 'payout-1' } as const;

interface FakePopup {
  closed: boolean;
  close: () => void;
  focus: () => void;
  /** What `window.open` was called with: the action page reads its state from it. */
  href?: string;
}

function fakePopup(): FakePopup {
  const popup: FakePopup = {
    closed: false,
    close: vi.fn(() => {
      popup.closed = true;
    }),
    focus: vi.fn(),
  };
  return popup;
}

// The one cast in this file: the fake stands in for a real `Window`, which jsdom
// cannot open. The helper reads only `closed`, `close()` and `focus()`, and
// compares the object by identity against `MessageEvent.source`.
function asWindow(popup: FakePopup): Window {
  return popup as unknown as Window;
}

function stubOpen(...popups: (FakePopup | null)[]) {
  const open = vi.spyOn(window, 'open');
  for (const popup of popups) {
    open.mockImplementationOnce(href => {
      if (popup === null) return null;
      popup.href = String(href);
      return asWindow(popup);
    });
  }
  return open;
}

function post(data: unknown, from: { origin: string; source: FakePopup }): void {
  window.dispatchEvent(new MessageEvent('message', { data, origin: from.origin, source: asWindow(from.source) }));
}

/** The state `popup` was opened with, as the action page reads it from its URL. */
function stateOf(popup: FakePopup): string | null {
  return new URL(popup.href ?? 'about:blank').searchParams.get('state');
}

/** `data` as the action page in `popup` posts it: echoing that popup's state. */
function answer(popup: FakePopup, data: Record<string, unknown> = RESULT, origin = 'https://evinvest.ltd'): void {
  post({ ...data, state: stateOf(popup) }, { origin, source: popup });
}

let openWalletAction: WalletModule['openWalletAction'];

beforeEach(async () => {
  vi.resetModules();
  ({ openWalletAction } = await import('../src/index'));
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('openWalletAction: refused before opening', () => {
  it('should_return_invalid_url_and_not_open_a_foreign_url', async () => {
    const open = stubOpen(fakePopup());

    await expect(openWalletAction('https://evil.example/wallet/withdraw')).resolves.toEqual({ status: 'invalid_url' });
    expect(open).not.toHaveBeenCalled();
  });

  it('should_return_invalid_url_and_not_open_a_wallet_path_that_is_not_an_action', async () => {
    const open = stubOpen(fakePopup());

    await expect(openWalletAction('https://evinvest.ltd/wallet/transfer')).resolves.toEqual({ status: 'invalid_url' });
    expect(open).not.toHaveBeenCalled();
  });

  it('should_return_aborted_and_not_open_when_the_signal_is_already_aborted', async () => {
    const open = stubOpen(fakePopup());
    const controller = new AbortController();
    controller.abort();

    await expect(openWalletAction(ACTION, { signal: controller.signal })).resolves.toEqual({ status: 'aborted' });
    expect(open).not.toHaveBeenCalled();
  });

  it('should_still_open_with_a_state_when_randomUUID_is_missing', () => {
    // An http page off loopback or an old browser has getRandomValues but no randomUUID.
    const open = stubOpen(fakePopup());
    vi.stubGlobal('crypto', { getRandomValues: crypto.getRandomValues.bind(crypto) });

    void openWalletAction(ACTION);

    expect(open).toHaveBeenCalledTimes(1);
    expect(new URL(String(open.mock.calls[0]?.[0])).searchParams.get('state')).toMatch(/^[0-9a-f]{32}$/);
  });

  it('should_return_unsupported_and_not_open_without_crypto', async () => {
    const open = stubOpen(fakePopup());
    vi.stubGlobal('crypto', {});

    await expect(openWalletAction(ACTION)).resolves.toEqual({ status: 'unsupported' });
    expect(open).not.toHaveBeenCalled();
  });

  it('should_return_blocked_when_window_open_is_refused', async () => {
    stubOpen(null);

    await expect(openWalletAction(ACTION)).resolves.toEqual({ status: 'blocked' });
  });

  it('should_open_on_the_next_call_after_a_blocked_one', async () => {
    const popup = fakePopup();
    const open = stubOpen(null, popup);
    await openWalletAction(ACTION);

    void openWalletAction(ACTION);

    expect(open).toHaveBeenCalledTimes(2);
  });
});

describe('openWalletAction: the popup', () => {
  it('should_open_the_action_with_this_origin_as_opener_in_the_named_wallet_window', () => {
    const open = stubOpen(fakePopup());

    vi.spyOn(crypto, 'getRandomValues').mockImplementation(<T extends ArrayBufferView | null>(array: T): T => {
      if (array instanceof Uint8Array) array.fill(0xab);
      return array;
    });

    void openWalletAction(new URL(ACTION));

    expect(open).toHaveBeenCalledWith(
      'https://evinvest.ltd/wallet/pay?invoice=inv-1&opener=http%3A%2F%2Flocalhost%3A3000&state=abababababababababababababababab',
      'ev-wallet-action',
      'popup=yes,width=480,height=720',
    );
  });

  it('should_open_the_rebuilt_action_without_parameters_outside_the_allow_list', () => {
    const popup = fakePopup();
    stubOpen(popup);

    void openWalletAction('https://evinvest.ltd/wallet/pay?invoice=inv-1&amount=999&opener=https://evil.example#x');

    const opened = new URL(popup.href ?? 'about:blank');
    expect([...opened.searchParams.keys()]).toEqual(['invoice', 'opener', 'state']);
    expect(opened.searchParams.get('opener')).toBe('http://localhost:3000');
    expect(opened.hash).toBe('');
  });

  it('should_open_each_action_with_a_fresh_state', async () => {
    const first = fakePopup();
    const second = fakePopup();
    stubOpen(first, second);
    const done = openWalletAction(ACTION);
    answer(first);
    await done;

    void openWalletAction(ACTION);

    expect(stateOf(second)).not.toBe(stateOf(first));
  });

  it('should_ignore_a_result_from_the_popup_without_the_state', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION);

    post({ ...RESULT, reference: 'no-state' }, { origin: 'https://evinvest.ltd', source: popup });
    answer(popup);

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
  });

  it('should_ignore_a_result_from_the_popup_carrying_another_actions_state', async () => {
    // A page reloaded from history in the same named window still holds the
    // state of the action that first opened it.
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION);

    post({ ...RESULT, state: 'stale-state', reference: 'stale' }, { origin: 'https://evinvest.ltd', source: popup });
    answer(popup);

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
  });

  it('should_time_out_when_the_popup_only_ever_posts_results_without_the_state', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION, { timeoutMs: 1_000 });

    post(RESULT, { origin: 'https://evinvest.ltd', source: popup });
    vi.advanceTimersByTime(1_000);

    await expect(outcome).resolves.toEqual({ status: 'timeout' });
  });

  it('should_resolve_with_the_result_and_close_the_popup', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION);

    answer(popup);

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
    expect(popup.close).toHaveBeenCalledTimes(1);
  });

  it('should_ignore_a_result_from_another_origin', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION);

    // With the right state, so only the origin check stands in the way.
    post({ ...RESULT, state: stateOf(popup), reference: 'forged' }, { origin: 'https://evil.example', source: popup });
    answer(popup);

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
  });

  it('should_ignore_a_result_from_another_evinvest_window', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION);

    // With the right state, so only the source check stands in the way.
    post({ ...RESULT, state: stateOf(popup), reference: 'other-tab' }, { origin: 'https://evinvest.ltd', source: fakePopup() });
    answer(popup);

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
  });

  it('should_ignore_a_message_from_the_popup_that_is_not_a_result', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION, { timeoutMs: 1_000 });

    post({ type: 'ev:wallet-progress' }, { origin: 'https://evinvest.ltd', source: popup });
    vi.advanceTimersByTime(1_000);

    await expect(outcome).resolves.toEqual({ status: 'timeout' });
  });

  it('should_accept_a_result_from_a_staging_origin_when_given_one', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction('https://stage.evinvest.ltd/wallet/pay', { origin: 'https://stage.evinvest.ltd' });

    answer(popup, RESULT, 'https://stage.evinvest.ltd');

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
  });
});

describe('openWalletAction: one action per page', () => {
  it('should_focus_the_open_popup_and_not_open_another_while_one_is_in_flight', async () => {
    const popup = fakePopup();
    const open = stubOpen(popup, fakePopup());
    void openWalletAction(ACTION);

    await expect(openWalletAction('https://evinvest.ltd/wallet/withdraw')).resolves.toEqual({ status: 'busy' });
    expect(open).toHaveBeenCalledTimes(1);
    expect(popup.focus).toHaveBeenCalledTimes(1);
  });

  it('should_keep_the_first_action_answering_after_a_busy_call', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const first = openWalletAction(ACTION);
    await openWalletAction(ACTION);

    answer(popup);

    await expect(first).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
  });

  it('should_open_a_new_popup_once_the_previous_action_settled', async () => {
    const first = fakePopup();
    const second = fakePopup();
    const open = stubOpen(first, second);
    const done = openWalletAction(ACTION);
    answer(first);
    await done;

    const next = openWalletAction('https://evinvest.ltd/wallet/withdraw');
    answer(second, { ...RESULT, reference: 'withdraw-1' });

    await expect(next).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'withdraw-1' } });
    expect(open).toHaveBeenCalledTimes(2);
  });

  it('should_free_the_page_on_settle_even_when_the_popup_stays_open', async () => {
    const stuck: FakePopup = { closed: false, close: vi.fn(), focus: vi.fn() };
    const open = stubOpen(stuck, fakePopup());
    const timedOut = openWalletAction(ACTION, { timeoutMs: 1_000 });
    vi.advanceTimersByTime(1_000);
    await timedOut;

    void openWalletAction(ACTION);

    expect(open).toHaveBeenCalledTimes(2);
    expect(stuck.focus).not.toHaveBeenCalled();
  });

  it('should_open_a_new_popup_when_the_previous_one_was_closed_but_not_yet_settled', async () => {
    const first = fakePopup();
    const open = stubOpen(first, fakePopup());
    void openWalletAction(ACTION);

    first.closed = true;
    void openWalletAction(ACTION);

    expect(open).toHaveBeenCalledTimes(2);
  });

  it('should_not_let_a_settling_old_action_free_the_new_one', async () => {
    const first = fakePopup();
    const second = fakePopup();
    const open = stubOpen(first, second, fakePopup());
    const old = openWalletAction(ACTION, { pollMs: 200 });
    first.closed = true;
    void openWalletAction(ACTION);
    vi.advanceTimersByTime(200);
    vi.advanceTimersToNextTimer();
    await expect(old).resolves.toEqual({ status: 'closed' });

    await expect(openWalletAction(ACTION)).resolves.toEqual({ status: 'busy' });
    expect(open).toHaveBeenCalledTimes(2);
    expect(second.focus).toHaveBeenCalledTimes(1);
  });
});

describe('openWalletAction: other ways out', () => {
  it('should_resolve_closed_once_the_poll_sees_the_popup_closed', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION, { pollMs: 200 });

    popup.closed = true;
    vi.advanceTimersByTime(200);
    // The poll defers `closed` by one more timer, so a result already on its way wins.
    vi.advanceTimersToNextTimer();

    await expect(outcome).resolves.toEqual({ status: 'closed' });
    expect(popup.close).not.toHaveBeenCalled();
  });

  it('should_not_resolve_closed_before_the_poll_runs', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const settled: WalletActionOutcome[] = [];
    void openWalletAction(ACTION, { pollMs: 200 }).then(outcome => settled.push(outcome));

    popup.closed = true;
    await vi.advanceTimersByTimeAsync(199);

    expect(settled).toEqual([]);
  });

  it('should_let_a_result_posted_just_before_closing_win_over_closed', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION, { pollMs: 200 });

    popup.closed = true;
    vi.advanceTimersToNextTimer();
    answer(popup);
    vi.advanceTimersToNextTimer();

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
  });

  it('should_resolve_timeout_and_close_the_popup_after_timeoutMs', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION, { timeoutMs: 5_000 });

    vi.advanceTimersByTime(5_000);

    await expect(outcome).resolves.toEqual({ status: 'timeout' });
    expect(popup.close).toHaveBeenCalledTimes(1);
  });

  it('should_time_out_after_fifteen_minutes_by_default', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const settled: WalletActionOutcome[] = [];
    const outcome = openWalletAction(ACTION);
    void outcome.then(value => settled.push(value));

    await vi.advanceTimersByTimeAsync(15 * 60_000 - 1);
    expect(settled).toEqual([]);
    vi.advanceTimersByTime(1);

    await expect(outcome).resolves.toEqual({ status: 'timeout' });
  });

  it('should_resolve_aborted_and_close_the_popup_on_abort', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const controller = new AbortController();
    const outcome = openWalletAction(ACTION, { signal: controller.signal });

    controller.abort();

    await expect(outcome).resolves.toEqual({ status: 'aborted' });
    expect(popup.close).toHaveBeenCalledTimes(1);
  });

  it('should_leave_no_timers_running_once_settled', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION);

    answer(popup);
    await outcome;

    expect(vi.getTimerCount()).toBe(0);
  });

  it('should_keep_the_first_outcome_when_a_second_result_arrives', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const outcome = openWalletAction(ACTION);

    answer(popup);
    answer(popup, { ...RESULT, outcome: 'failed' });

    await expect(outcome).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: 'payout-1' } });
    expect(popup.close).toHaveBeenCalledTimes(1);
  });
});

describe('walletNavigationHref', () => {
  // jsdom serves this page from http://localhost:3000, a loopback dev stand.
  afterEach(() => {
    window.history.replaceState(null, '', '/');
  });

  it('should_return_to_this_page_without_its_query_or_hash_by_default', () => {
    window.history.replaceState(null, '', '/billing?token=secret#row-7');

    const href = walletNavigationHref(new URL(ACTION));

    expect(href).toBe('https://evinvest.ltd/wallet/pay?invoice=inv-1&return_to=http%3A%2F%2Flocalhost%3A3000%2Fbilling');
  });

  it('should_cut_a_same_origin_return_to_to_origin_and_path', () => {
    const href = walletNavigationHref(new URL(ACTION), 'http://localhost:3000/billing?x=1#row-7');

    expect(href).toBe('https://evinvest.ltd/wallet/pay?invoice=inv-1&return_to=http%3A%2F%2Flocalhost%3A3000%2Fbilling');
  });

  it('should_resolve_a_relative_return_to_against_this_page', () => {
    const href = walletNavigationHref(new URL(ACTION), '/billing');

    expect(href).toBe('https://evinvest.ltd/wallet/pay?invoice=inv-1&return_to=http%3A%2F%2Flocalhost%3A3000%2Fbilling');
  });

  it('should_replace_a_return_to_on_another_origin_with_this_page', () => {
    window.history.replaceState(null, '', '/account');

    const href = walletNavigationHref(new URL(ACTION), 'https://sa.evinvest.ltd/billing?x=1');

    expect(href).toBe('https://evinvest.ltd/wallet/pay?invoice=inv-1&return_to=http%3A%2F%2Flocalhost%3A3000%2Faccount');
  });

  it('should_replace_a_plain_http_return_to_off_loopback_with_this_page', () => {
    // Same origin, so only the transport check can refuse it.
    vi.stubGlobal('location', { href: 'http://sa.example/account', origin: 'http://sa.example', pathname: '/account' });

    const href = walletNavigationHref(new URL(ACTION), 'http://sa.example/billing');

    expect(href).toBe('https://evinvest.ltd/wallet/pay?invoice=inv-1&return_to=http%3A%2F%2Fsa.example%2Faccount');
  });

  it('should_rebuild_the_action_without_parameters_outside_the_allow_list', () => {
    const href = walletNavigationHref(new URL(`${ACTION}&amount=999&return_to=https://evil.example/`), '/billing');

    expect(href).toBe('https://evinvest.ltd/wallet/pay?invoice=inv-1&return_to=http%3A%2F%2Flocalhost%3A3000%2Fbilling');
  });

  it('should_return_null_for_a_wallet_path_that_is_not_an_action', () => {
    expect(walletNavigationHref(new URL('https://evinvest.ltd/wallet/transfer'), '/billing')).toBeNull();
  });

  it('should_take_a_staging_action_when_given_the_staging_origin', () => {
    const href = walletNavigationHref(new URL('https://stage.evinvest.ltd/wallet/pay?invoice=inv-1'), '/billing', 'https://stage.evinvest.ltd');

    expect(href).toBe('https://stage.evinvest.ltd/wallet/pay?invoice=inv-1&return_to=http%3A%2F%2Flocalhost%3A3000%2Fbilling');
  });

  it('should_return_null_for_a_staging_action_without_the_staging_origin', () => {
    expect(walletNavigationHref(new URL('https://stage.evinvest.ltd/wallet/pay?invoice=inv-1'), '/billing')).toBeNull();
  });
});

describe('openWalletAction: the payment answer', () => {
  const PAYMENT_ID = '4f0c2d1e-0000-4000-8000-0123456789ab';

  function paymentUrl(): URL {
    const url = walletPaymentUrl(PAYMENT_ID, 'invest');
    if (!url) throw new Error('walletPaymentUrl returned null for a valid id');
    return url;
  }

  it('should_open_the_payment_page_with_opener_and_state_added', () => {
    const popup = fakePopup();
    stubOpen(popup);

    void openWalletAction(paymentUrl());

    const opened = new URL(popup.href ?? 'about:blank');
    expect(opened.origin + opened.pathname).toBe(`https://evinvest.ltd/wallet/payment/${PAYMENT_ID}`);
    expect([...opened.searchParams.keys()]).toEqual(['scope', 'opener', 'state']);
    expect(opened.searchParams.get('scope')).toBe('invest');
    expect(opened.searchParams.get('opener')).toBe('http://localhost:3000');
    expect(opened.searchParams.get('state')).toMatch(/^[0-9a-f]{32}$/);
  });

  it('should_resolve_the_result_when_the_payment_page_posts_it', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    const done = openWalletAction(paymentUrl());

    answer(popup, { type: 'ev:wallet-result', outcome: 'done', reference: PAYMENT_ID });

    await expect(done).resolves.toEqual({ status: 'result', result: { outcome: 'done', reference: PAYMENT_ID } });
    expect(popup.close).toHaveBeenCalled();
  });

  it('should_ignore_a_payment_result_that_echoes_another_state', async () => {
    const popup = fakePopup();
    stubOpen(popup);
    let outcome: WalletActionOutcome | undefined;
    void openWalletAction(paymentUrl()).then(value => {
      outcome = value;
    });

    post({ ...RESULT, state: 'someone-elses', reference: PAYMENT_ID }, { origin: 'https://evinvest.ltd', source: popup });
    await vi.advanceTimersByTimeAsync(0);

    expect(outcome).toBeUndefined();
  });

  it('should_build_the_navigation_href_of_a_payment_url', () => {
    const href = walletNavigationHref(paymentUrl(), '/billing');

    expect(href).toBe(`https://evinvest.ltd/wallet/payment/${PAYMENT_ID}?scope=invest&return_to=http%3A%2F%2Flocalhost%3A3000%2Fbilling`);
  });
});
