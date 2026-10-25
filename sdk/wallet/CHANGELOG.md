# Changelog

All notable changes to `@evinvest/wallet` are in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the package uses
[Semantic Versioning](https://semver.org/).

## [0.2.0] - Unreleased

Release banking's wallet bundle of contract version 3 before this version. A
host on 0.2.0 refuses an older element and shows its fallback.

### Added

- Inline mode: `mode="inline"` on `Wallet`, `PayInvoiceButton` and
  `MandateButton` sets `actions="inline"` on the element. The element then runs
  an action in the host page when banking allows that kind for the host. Its
  `ev:wallet-result` event reaches `onResult` as `{ status: 'result', result }`,
  with `result.kind`. Other actions still open the popup.
- `parseWalletResultEvent`, `WALLET_RESULT_EVENT`, `INLINE_ACTIONS_ATTRIBUTE`,
  `WALLET_INLINE_KINDS` for hosts without React.
- `MandateButton` and the `mfe-banking-mandate` element (`banking.mandate`).
- `walletCsp()`, `serializeCsp()`, `mergeCsp()`: the CSP sources the wallet
  needs in popup and inline mode.
- Integrity helpers: `parseWalletPins`, `pinToSri`, `walletSri`,
  `verifyWalletBundle`, `walletImportMap`.
- `WALLET_API_VERSION` (3), the wallet's own contract version.

### Changed

- The React wrappers default `minApiVersion` to `WALLET_API_VERSION`, so an
  element of contract version 2 is refused.
- `WalletActionOutcome`'s `result` may be a `WalletInlineResult`, which adds
  `kind`.

## [0.1.2] - 2026-10-22

### Added

- `walletPaymentUrl` and the `/wallet/payment/<id>` action: the user's answer
  to a payment out of their account.
- The `request` query parameter for a mandate request.

## [0.1.1] - 2026-10-14

### Changed

- The first release from the banking repository, with provenance. The code is
  the same as 0.1.0.

## [0.1.0]

### Added

- The first release, from the `lib` repository: `Wallet`, `PayInvoiceButton`
  and the popup round trip (`openWalletAction`).
