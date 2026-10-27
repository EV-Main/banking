// The host owns its CSP; these helpers only list the wallet's sources and add them
// without breaking what the host already allows. Origins land in a header, so anything
// but an exact lowercase https origin is refused.
import { describe, expect, it } from 'vitest';

import { mergeCsp, serializeCsp, walletCsp } from '../src/index';

describe('walletCsp', () => {
  it('should_list_the_bundle_directory_and_read_routes_in_popup_mode', () => {
    expect(walletCsp()).toEqual({
      'script-src': ['https://evinvest.ltd/wallet/mfe/'],
      'style-src': ['https://evinvest.ltd/wallet/mfe/'],
      'connect-src': [
        'https://evinvest.ltd/wallet/mfe/',
        'https://evinvest.ltd/api/auth/session',
        'https://evinvest.ltd/cabinet/api/wallet',
        'https://evinvest.ltd/cabinet/api/wallet/csrf',
        'https://evinvest.ltd/cabinet/api/operations',
      ],
    });
  });

  it('should_add_the_intent_directory_and_the_step_up_element_in_inline_mode', () => {
    const csp = walletCsp({ mode: 'inline' });

    expect(csp['script-src']).toEqual(['https://evinvest.ltd/wallet/mfe/', 'https://evinvest.ltd/api/auth/mfe/']);
    expect(csp['style-src']).toEqual(['https://evinvest.ltd/wallet/mfe/', 'https://evinvest.ltd/api/auth/mfe/']);
    expect(csp['connect-src']).toEqual([
      'https://evinvest.ltd/wallet/mfe/',
      'https://evinvest.ltd/api/auth/session',
      'https://evinvest.ltd/cabinet/api/wallet',
      'https://evinvest.ltd/cabinet/api/wallet/csrf',
      'https://evinvest.ltd/cabinet/api/operations',
      'https://evinvest.ltd/api/auth/mfe/',
      'https://evinvest.ltd/cabinet/api/wallet/',
      'https://evinvest.ltd/api/auth/stepup/',
    ]);
  });

  it('should_return_frozen_directives', () => {
    const csp = walletCsp();

    expect(Object.isFrozen(csp)).toBe(true);
    expect(Object.isFrozen(csp['connect-src'])).toBe(true);
  });

  it('should_accept_a_staging_origin_with_a_port', () => {
    expect(walletCsp({ origin: 'https://x.y:8443' })['script-src']).toEqual(['https://x.y:8443/wallet/mfe/']);
  });

  it.each([
    ['uppercase', 'https://X.y'],
    ['http', 'http://evinvest.ltd'],
    ['a trailing slash', 'https://evinvest.ltd/'],
    ['a path', 'https://evinvest.ltd/wallet'],
    ['a wildcard', 'https://*.evinvest.ltd'],
    ['credentials', 'https://user@evinvest.ltd'],
    ['a space', 'https://evinvest.ltd https://evil.example'],
    ['a default port', 'https://evinvest.ltd:443'],
  ])('should_throw_for_an_origin_with_%s', (_label, origin) => {
    expect(() => walletCsp({ origin })).toThrow(TypeError);
  });
});

describe('serializeCsp', () => {
  it('should_sort_directives_keep_source_order_and_drop_duplicates', () => {
    expect(serializeCsp({ 'script-src': ["'self'", 'https://a.example', "'self'"], 'connect-src': ["'self'"] })).toBe(
      "connect-src 'self'; script-src 'self' https://a.example",
    );
  });

  it('should_write_a_directive_without_sources_as_its_bare_name', () => {
    expect(serializeCsp({ 'object-src': [], 'default-src': ["'self'"] })).toBe("default-src 'self'; object-src");
  });

  it.each([
    ['a semicolon', "'self'; script-src *"],
    ['a space', "'self' https://evil.example"],
    ['a comma', "'self',default-src *"],
  ])('should_throw_for_a_source_with_%s', (_label, source) => {
    expect(() => serializeCsp({ 'script-src': [source] })).toThrow(TypeError);
  });

  it('should_throw_for_a_bad_directive_name', () => {
    expect(() => serializeCsp({ 'Script-Src; x': ["'self'"] })).toThrow(TypeError);
  });
});

describe('mergeCsp', () => {
  const wallet = walletCsp();

  it('should_append_wallet_sources_after_the_host_sources_without_repeating_them', () => {
    const merged = mergeCsp({ 'script-src': ["'self'", 'https://evinvest.ltd/wallet/mfe/'] }, { 'script-src': ['https://evinvest.ltd/wallet/mfe/', 'https://b.example'] });

    expect(merged['script-src']).toEqual(["'self'", 'https://evinvest.ltd/wallet/mfe/', 'https://b.example']);
  });

  it('should_seed_a_missing_fetch_directive_from_default_src', () => {
    const merged = mergeCsp({ 'default-src': ["'self'"] }, wallet);

    expect(merged['default-src']).toEqual(["'self'"]);
    expect(merged['connect-src']).toEqual([
      "'self'",
      'https://evinvest.ltd/wallet/mfe/',
      'https://evinvest.ltd/api/auth/session',
      'https://evinvest.ltd/cabinet/api/wallet',
      'https://evinvest.ltd/cabinet/api/wallet/csrf',
      'https://evinvest.ltd/cabinet/api/operations',
    ]);
    expect(merged['script-src']).toEqual(["'self'", 'https://evinvest.ltd/wallet/mfe/']);
  });

  it('should_add_no_wallet_directive_where_the_host_restricts_nothing', () => {
    // Adding `script-src` to this policy would block every other script the host loads.
    const merged = mergeCsp({ 'img-src': ["'self'"] }, wallet);

    expect(merged).toEqual({ 'img-src': ["'self'"] });
  });

  it('should_drop_none_when_a_source_is_added_beside_it', () => {
    const merged = mergeCsp({ 'connect-src': ["'none'"] }, { 'connect-src': ['https://evinvest.ltd/api/auth/session'] });

    expect(merged['connect-src']).toEqual(['https://evinvest.ltd/api/auth/session']);
  });

  it('should_keep_none_when_it_is_the_only_source', () => {
    const merged = mergeCsp({ 'connect-src': ["'none'"] }, { 'connect-src': [] });

    expect(merged['connect-src']).toEqual(["'none'"]);
  });

  it('should_give_style_src_elem_the_style_src_sources_when_the_host_sets_it', () => {
    const merged = mergeCsp({ 'style-src': ["'self'"], 'style-src-elem': ["'self'"] }, wallet);

    expect(merged['style-src-elem']).toEqual(["'self'", 'https://evinvest.ltd/wallet/mfe/']);
  });

  it('should_give_script_src_elem_the_script_src_sources_when_the_host_sets_it', () => {
    const merged = mergeCsp({ 'default-src': ["'self'"], 'script-src-elem': ["'self'", "'nonce-abc'"] }, wallet);

    expect(merged['script-src-elem']).toEqual(["'self'", "'nonce-abc'", 'https://evinvest.ltd/wallet/mfe/']);
  });

  it('should_not_create_an_element_directive_the_host_does_not_set', () => {
    const merged = mergeCsp({ 'script-src': ["'self'"] }, wallet);

    expect(merged).not.toHaveProperty('script-src-elem');
  });

  it('should_leave_the_host_policy_untouched_and_return_a_frozen_one', () => {
    const host = { 'script-src': ["'self'"] };

    const merged = mergeCsp(host, wallet);

    expect(host).toEqual({ 'script-src': ["'self'"] });
    expect(Object.isFrozen(merged)).toBe(true);
    expect(Object.isFrozen(merged['script-src'])).toBe(true);
  });

  it('should_serialize_a_merged_policy_into_one_header', () => {
    const merged = mergeCsp({ 'default-src': ["'self'"], 'img-src': ['data:'] }, walletCsp({ mode: 'inline' }));

    expect(serializeCsp(merged)).toBe(
      "connect-src 'self' https://evinvest.ltd/wallet/mfe/ https://evinvest.ltd/api/auth/session https://evinvest.ltd/cabinet/api/wallet https://evinvest.ltd/cabinet/api/wallet/csrf https://evinvest.ltd/cabinet/api/operations https://evinvest.ltd/api/auth/mfe/ https://evinvest.ltd/cabinet/api/wallet/ https://evinvest.ltd/api/auth/stepup/; default-src 'self'; img-src data:; script-src 'self' https://evinvest.ltd/wallet/mfe/ https://evinvest.ltd/api/auth/mfe/; style-src 'self' https://evinvest.ltd/wallet/mfe/ https://evinvest.ltd/api/auth/mfe/",
    );
  });
});
