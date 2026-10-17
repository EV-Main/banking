// A money action leaves the scope host for evinvest.ltd, so the action URL is
// checked the way a browser will read it and then rebuilt: only the four known
// action paths, only `invoice` and `scope`, on the wallet origin, with no
// credentials, backslashes or control characters. Whatever else the element
// put in the href — an amount, a recipient, a return address — never reaches
// the action page. The result message comes back over a shared channel and
// counts only when it echoes the state this action was opened with.
import { describe, expect, it } from 'vitest';

import {
  parseWalletMessage,
  WALLET_ACTION_PARAMS,
  WALLET_ACTION_PATHS,
  walletActionFromEvent,
  walletActionUrl,
} from '../src/index';

const STATE = 'state-1';

describe('wallet action allow-lists', () => {
  it('should_name_exactly_the_four_money_actions', () => {
    expect(WALLET_ACTION_PATHS).toEqual(['/wallet/pay', '/wallet/withdraw', '/wallet/move', '/wallet/mandate']);
  });

  it('should_keep_only_invoice_scope_and_request', () => {
    expect(WALLET_ACTION_PARAMS).toEqual(['invoice', 'scope', 'request']);
  });
});

describe('walletActionUrl: what passes', () => {
  it.each(['pay', 'withdraw', 'move', 'mandate'])('should_accept_the_%s_action_on_evinvest_ltd', action => {
    expect(walletActionUrl(`https://evinvest.ltd/wallet/${action}`)?.href).toBe(`https://evinvest.ltd/wallet/${action}`);
  });

  it('should_keep_the_request_parameter_of_a_mandate_page', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/mandate?request=mr_1&scope=sa&limit=9')?.href).toBe(
      'https://evinvest.ltd/wallet/mandate?scope=sa&request=mr_1',
    );
  });

  it.each(['a/b', 'a%2Fb', 'a b', 'a.b', ''])('should_refuse_the_whole_url_for_a_request_%j', request => {
    expect(walletActionUrl(`https://evinvest.ltd/wallet/mandate?request=${request}`)).toBeNull();
  });

  it('should_keep_the_invoice_and_scope_parameters', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/pay?invoice=inv-1&scope=sa')?.href).toBe(
      'https://evinvest.ltd/wallet/pay?invoice=inv-1&scope=sa',
    );
  });

  it('should_drop_every_parameter_outside_the_allow_list', () => {
    const url = walletActionUrl('https://evinvest.ltd/wallet/withdraw?amount=5&to=EQabc&opener=https://evil.example&return_to=https://evil.example&state=s');

    expect(url?.href).toBe('https://evinvest.ltd/wallet/withdraw');
  });

  it('should_drop_an_extra_parameter_and_keep_the_invoice', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/pay?invoice=inv-1&amount=999')?.href).toBe(
      'https://evinvest.ltd/wallet/pay?invoice=inv-1',
    );
  });

  it('should_drop_the_hash', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/pay?invoice=inv-1#amount=999')?.href).toBe(
      'https://evinvest.ltd/wallet/pay?invoice=inv-1',
    );
  });

  it('should_keep_only_the_first_value_of_a_repeated_parameter', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/pay?invoice=inv-1&invoice=inv-2')?.href).toBe(
      'https://evinvest.ltd/wallet/pay?invoice=inv-1',
    );
  });

  it('should_keep_an_invoice_of_exactly_128_id_characters', () => {
    const invoice = `${'Ab9_-'.repeat(25)}xyz`;
    expect(invoice).toHaveLength(128);


    expect(walletActionUrl(`https://evinvest.ltd/wallet/pay?invoice=${invoice}`)?.href).toBe(`https://evinvest.ltd/wallet/pay?invoice=${invoice}`);
  });

  it('should_accept_a_staging_origin_when_given_one', () => {
    const url = walletActionUrl('https://stage.evinvest.ltd/wallet/pay', 'https://stage.evinvest.ltd');

    expect(url?.href).toBe('https://stage.evinvest.ltd/wallet/pay');
  });

  it('should_accept_a_url_of_exactly_2048_characters_and_drop_its_padding', () => {
    const prefix = 'https://evinvest.ltd/wallet/pay?invoice=inv-1&pad=';
    const raw = `${prefix}${'a'.repeat(2048 - prefix.length)}`;
    expect(raw).toHaveLength(2048);

    expect(walletActionUrl(raw)?.href).toBe('https://evinvest.ltd/wallet/pay?invoice=inv-1');
  });
});

describe('walletActionUrl: what is refused', () => {
  // An id the page would look up differently from how the element named it is
  // refused whole, never cleaned: dropping it would open a different action.
  it.each([
    ['empty', ''],
    ['129 characters long', 'i'.repeat(129)],
    ['holding a slash', 'a/b'],
    ['holding an encoded slash', 'a%2F'],
    ['holding a dot', 'a.b'],
    ['holding a space', 'a b'],
  ])('should_refuse_the_whole_url_for_an_invoice_%s', (_label, invoice) => {
    expect(walletActionUrl(`https://evinvest.ltd/wallet/pay?invoice=${invoice}&scope=sa`)).toBeNull();
  });

  it.each([
    ['empty', ''],
    ['129 characters long', 's'.repeat(129)],
    ['holding a slash', 'a/b'],
    ['holding an encoded slash', 'a%2F'],
    ['holding a dot', 'a.b'],
  ])('should_refuse_the_whole_url_for_a_scope_%s', (_label, scope) => {
    expect(walletActionUrl(`https://evinvest.ltd/wallet/move?scope=${scope}`)).toBeNull();
  });

  it.each([
    ['another host', 'https://evil.example/wallet/withdraw'],
    ['a lookalike host', 'https://evinvest.ltd.evil.example/wallet/withdraw'],
    ['a subdomain', 'https://invest.evinvest.ltd/wallet/withdraw'],
    ['plain http', 'http://evinvest.ltd/wallet/withdraw'],
    ['another port', 'https://evinvest.ltd:8443/wallet/withdraw'],
  ])('should_refuse_%s', (_label, raw) => {
    expect(walletActionUrl(raw)).toBeNull();
  });

  it('should_refuse_the_production_origin_when_a_staging_origin_is_expected', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/pay', 'https://stage.evinvest.ltd')).toBeNull();
  });

  it.each([
    ['the root', 'https://evinvest.ltd/'],
    ['another section', 'https://evinvest.ltd/admin/users'],
    ['wallet without the trailing slash', 'https://evinvest.ltd/wallet'],
    ['the wallet directory itself', 'https://evinvest.ltd/wallet/'],
    ['a sibling prefix', 'https://evinvest.ltd/wallets/pay'],
  ])('should_refuse_a_path_outside_wallet: %s', (_label, raw) => {
    expect(walletActionUrl(raw)).toBeNull();
  });

  it.each([
    ['an unknown action', 'https://evinvest.ltd/wallet/transfer'],
    ['another wallet page', 'https://evinvest.ltd/wallet/history'],
    ['the bundle directory', 'https://evinvest.ltd/wallet/mfe/wallet.js'],
    ['a trailing slash', 'https://evinvest.ltd/wallet/pay/'],
    ['a deeper path', 'https://evinvest.ltd/wallet/pay/confirm'],
    ['another case', 'https://evinvest.ltd/wallet/PAY'],
    ['a percent-encoded letter', 'https://evinvest.ltd/wallet/%70ay'],
    ['a doubled slash', 'https://evinvest.ltd/wallet//pay'],
  ])('should_refuse_a_wallet_path_that_is_not_an_action: %s', (_label, raw) => {
    expect(walletActionUrl(raw)).toBeNull();
  });

  it('should_refuse_dot_segments_that_resolve_outside_wallet', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/../admin')).toBeNull();
  });

  it('should_refuse_percent_encoded_dot_segments_that_resolve_outside_wallet', () => {
    expect(walletActionUrl('https://evinvest.ltd/wallet/%2e%2e/admin')).toBeNull();
  });

  it.each([
    ['user and password', 'https://user:pass@evinvest.ltd/wallet/withdraw'],
    ['user only', 'https://user@evinvest.ltd/wallet/withdraw'],
    ['password only', 'https://:pass@evinvest.ltd/wallet/withdraw'],
  ])('should_refuse_credentials_in_the_url: %s', (_label, raw) => {
    expect(walletActionUrl(raw)).toBeNull();
  });

  it.each([
    ['in the path', 'https://evinvest.ltd/wallet/\\..\\admin'],
    ['before the path', 'https://evinvest.ltd\\wallet\\withdraw'],
    ['in the query', 'https://evinvest.ltd/wallet/withdraw?to=a\\b'],
  ])('should_refuse_a_backslash_%s', (_label, raw) => {
    expect(walletActionUrl(raw)).toBeNull();
  });

  it.each([
    ['tab', 'https://evinvest.ltd/wal\tlet/withdraw'],
    ['newline', 'https://evinvest.ltd/wallet/\n../admin'],
    ['nul', 'https://evinvest.ltd/wallet/pay\u0000'],
    ['delete', 'https://evinvest.ltd/wallet/pay\u007F'],
  ])('should_refuse_a_%s_character', (_label, raw) => {
    expect(walletActionUrl(raw)).toBeNull();
  });

  it('should_refuse_a_url_longer_than_2048_characters', () => {
    const prefix = 'https://evinvest.ltd/wallet/pay?invoice=inv-1&pad=';
    const raw = `${prefix}${'a'.repeat(2049 - prefix.length)}`;
    expect(raw).toHaveLength(2049);

    expect(walletActionUrl(raw)).toBeNull();
  });

  it.each([
    ['empty', ''],
    ['relative', '/wallet/withdraw'],
    ['javascript', 'javascript:alert(1)'],
    ['a number', 42],
    ['undefined', undefined],
    ['an object', { href: 'https://evinvest.ltd/wallet/pay' }],
  ])('should_refuse_input_that_is_%s', (_label, raw) => {
    expect(walletActionUrl(raw)).toBeNull();
  });
});

describe('walletActionFromEvent', () => {
  it('should_read_the_action_url_from_the_event_detail', () => {
    const event = new CustomEvent('ev:wallet-action', { detail: { href: 'https://evinvest.ltd/wallet/move?scope=sa&amount=5' } });

    expect(walletActionFromEvent(event)?.href).toBe('https://evinvest.ltd/wallet/move?scope=sa');
  });

  it('should_return_null_for_a_detail_with_a_wallet_path_that_is_not_an_action', () => {
    const event = new CustomEvent('ev:wallet-action', { detail: { href: 'https://evinvest.ltd/wallet/transfer' } });

    expect(walletActionFromEvent(event)).toBeNull();
  });

  it('should_return_null_for_a_detail_with_a_foreign_href', () => {
    const event = new CustomEvent('ev:wallet-action', { detail: { href: 'https://evil.example/wallet/transfer' } });

    expect(walletActionFromEvent(event)).toBeNull();
  });

  it('should_return_null_for_a_missing_detail_or_a_plain_event', () => {
    expect(walletActionFromEvent(new CustomEvent('ev:wallet-action'))).toBeNull();
    expect(walletActionFromEvent(new Event('ev:wallet-action'))).toBeNull();
  });
});

describe('parseWalletMessage: what passes', () => {
  it('should_read_a_done_result_with_a_reference', () => {
    expect(parseWalletMessage({ type: 'ev:wallet-result', state: STATE, outcome: 'done', reference: 'payout-1' }, STATE)).toEqual({
      outcome: 'done',
      reference: 'payout-1',
    });
  });

  it.each(['failed', 'cancelled'])('should_read_the_%s_outcome', outcome => {
    expect(parseWalletMessage({ type: 'ev:wallet-result', state: STATE, outcome }, STATE)).toEqual({ outcome, reference: null });
  });

  it('should_read_a_null_reference_as_null', () => {
    expect(parseWalletMessage({ type: 'ev:wallet-result', state: STATE, outcome: 'done', reference: null }, STATE)).toEqual({
      outcome: 'done',
      reference: null,
    });
  });

  it('should_accept_a_reference_of_exactly_128_characters', () => {
    const reference = 'r'.repeat(128);

    expect(parseWalletMessage({ type: 'ev:wallet-result', state: STATE, outcome: 'done', reference }, STATE)).toEqual({ outcome: 'done', reference });
  });

  // The outcome is what matters — money may already have moved — so an unusable
  // reference reads as none instead of dropping the whole result.
  it.each([
    ['empty', ''],
    ['129 characters long', 'r'.repeat(129)],
    ['a number', 7],
    ['an object', { id: 'p-1' }],
  ])('should_keep_the_outcome_and_drop_a_reference_that_is_%s', (_label, reference) => {
    expect(parseWalletMessage({ type: 'ev:wallet-result', state: STATE, outcome: 'done', reference }, STATE)).toEqual({
      outcome: 'done',
      reference: null,
    });
  });

  it('should_keep_a_long_reference_out_rather_than_cut_it', () => {
    expect(parseWalletMessage({ type: 'ev:wallet-result', state: STATE, outcome: 'failed', reference: 'p'.repeat(200) }, STATE)).toEqual({
      outcome: 'failed',
      reference: null,
    });
  });

  it('should_ignore_unknown_fields', () => {
    expect(parseWalletMessage({ type: 'ev:wallet-result', state: STATE, outcome: 'done', reference: 'p-1', amount: 5 }, STATE)).toStrictEqual({
      outcome: 'done',
      reference: 'p-1',
    });
  });
});

describe('parseWalletMessage: what is ignored', () => {
  it.each([
    ['another type', { type: 'ev:wallet-progress', state: STATE, outcome: 'done' }],
    ['no type', { state: STATE, outcome: 'done' }],
    ['an unknown outcome', { type: 'ev:wallet-result', state: STATE, outcome: 'success' }],
    ['no outcome', { type: 'ev:wallet-result', state: STATE }],
  ])('should_return_null_for_%s', (_label, data) => {
    expect(parseWalletMessage(data, STATE)).toBeNull();
  });

  it.each([null, undefined, 'ev:wallet-result', 1])('should_return_null_for_a_non_object: %j', data => {
    expect(parseWalletMessage(data, STATE)).toBeNull();
  });

  it.each([
    ['no state', { type: 'ev:wallet-result', outcome: 'done' }],
    ['another action\'s state', { type: 'ev:wallet-result', state: 'state-2', outcome: 'done' }],
    ['a state of another type', { type: 'ev:wallet-result', state: 1, outcome: 'done' }],
    ['a state that differs only in case', { type: 'ev:wallet-result', state: 'STATE-1', outcome: 'done' }],
  ])('should_return_null_for_a_result_with_%s', (_label, data) => {
    expect(parseWalletMessage(data, STATE)).toBeNull();
  });

  it('should_return_null_when_the_expected_state_is_empty_even_if_the_message_echoes_it', () => {
    expect(parseWalletMessage({ type: 'ev:wallet-result', state: '', outcome: 'done' }, '')).toBeNull();
  });
});
