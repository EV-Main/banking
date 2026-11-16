// The in-page result comes from a bundle the host does not build, so its detail is
// untrusted: anything that is not a known kind and outcome is no result at all, and an
// unusable reference reads as none without dropping the outcome.
import { describe, expect, it } from 'vitest';

import { parseWalletResultEvent, WALLET_RESULT_EVENT } from '../src/index';

function resultEvent(detail: unknown, type: string = WALLET_RESULT_EVENT): CustomEvent {
  return new CustomEvent(type, { detail });
}

describe('parseWalletResultEvent', () => {
  it('should_read_kind_outcome_and_reference', () => {
    expect(parseWalletResultEvent(resultEvent({ kind: 'pay', outcome: 'done', reference: 'payment-1' }))).toEqual({
      kind: 'pay',
      outcome: 'done',
      reference: 'payment-1',
    });
  });

  it.each(['move', 'withdraw', 'pay', 'mandate'])('should_accept_the_%s_kind', kind => {
    expect(parseWalletResultEvent(resultEvent({ kind, outcome: 'cancelled' }))).toEqual({ kind, outcome: 'cancelled', reference: null });
  });

  it.each([
    ['a number', 7],
    ['an empty string', ''],
    ['129 characters', 'r'.repeat(129)],
  ])('should_read_a_reference_that_is_%s_as_none_and_keep_the_outcome', (_label, reference) => {
    expect(parseWalletResultEvent(resultEvent({ kind: 'withdraw', outcome: 'done', reference }))).toEqual({
      kind: 'withdraw',
      outcome: 'done',
      reference: null,
    });
  });

  it('should_keep_a_reference_of_exactly_128_characters', () => {
    const reference = 'r'.repeat(128);

    expect(parseWalletResultEvent(resultEvent({ kind: 'move', outcome: 'failed', reference }))?.reference).toBe(reference);
  });

  it.each([
    ['an unknown kind', resultEvent({ kind: 'transfer', outcome: 'done' })],
    ['an unknown outcome', resultEvent({ kind: 'pay', outcome: 'success' })],
    ['no kind', resultEvent({ outcome: 'done' })],
    ['a null detail', resultEvent(null)],
    ['a string detail', resultEvent('done')],
    ['another event name', resultEvent({ kind: 'pay', outcome: 'done' }, 'ev:wallet-action')],
    ['a plain Event', new Event(WALLET_RESULT_EVENT)],
    ['a plain object', { type: WALLET_RESULT_EVENT, detail: { kind: 'pay', outcome: 'done' } }],
    ['undefined', undefined],
  ])('should_return_null_for_%s', (_label, event) => {
    expect(parseWalletResultEvent(event)).toBeNull();
  });
});
