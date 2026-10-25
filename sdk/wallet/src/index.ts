/**
 * @module @evinvest/wallet
 *
 * Host side of the banking wallet microfrontends (`Wallet`, `PayInvoiceButton`,
 * `MandateButton`): where they live, their tags and contract version, the popup
 * round trip that carries a money action to evinvest.ltd and brings its result
 * back, the inline mode in which the element runs an action in the host page,
 * the CSP sources a host must allow, and the integrity checks of the published
 * bundle. Server-safe: nothing touches the DOM at import time. The React
 * wrappers live in `./react`. Built on @evinvest/mfe; the elements themselves
 * are banking's. No iframe and no money logic here: the elements and banking's
 * routes decide what moves.
 */

export {
  fetchWalletRegistry,
  MANDATE_TAG,
  PAY_INVOICE_TAG,
  WALLET_API_VERSION,
  WALLET_ELEMENTS,
  WALLET_MFE_PREFIX,
  WALLET_ORIGIN,
  WALLET_REGISTRY_URL,
  WALLET_TAG,
  walletEntry,
} from './elements';
export type { WalletElement, WalletRegistryOptions } from './elements';

export {
  OPENER_PARAM,
  openWalletAction,
  parseWalletMessage,
  RETURN_TO_PARAM,
  STATE_PARAM,
  WALLET_ACTION_EVENT,
  WALLET_ACTION_PARAMS,
  WALLET_ACTION_PATHS,
  WALLET_RESULT_MESSAGE,
  walletActionFromEvent,
  walletActionUrl,
  walletNavigationHref,
  walletPaymentUrl,
} from './action';
export type { WalletActionOptions, WalletActionOutcome, WalletActionPath, WalletResult } from './action';

export {
  INLINE_ACTIONS_ATTRIBUTE,
  INLINE_ACTIONS_VALUE,
  parseWalletResultEvent,
  WALLET_INLINE_KINDS,
  WALLET_RESULT_EVENT,
} from './inline';
export type { WalletActionMode, WalletInlineKind, WalletInlineResult } from './inline';

export { mergeCsp, serializeCsp, walletCsp } from './csp';
export type { CspDirectives, WalletCsp, WalletCspOptions } from './csp';

export { parseWalletPins, pinToSri, verifyWalletBundle, WALLET_FILES, walletImportMap, walletSri } from './integrity';
export type {
  VerifyWalletBundleOptions,
  WalletBundleCheck,
  WalletFile,
  WalletFileCheck,
  WalletImportMap,
  WalletPins,
  WalletSriAlgorithm,
} from './integrity';
