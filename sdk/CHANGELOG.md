# Changelog

All notable changes to `@evinvest/banking` (up to 0.2.0 published as
`@evinvest/wallet`) are in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the package uses
[Semantic Versioning](https://semver.org/).

## [0.5.0] - 2026-11-16

### Removed

- `walletCsp({ mode: 'inline' })` no longer names concierge's old `/api/auth/mfe/`.
  Banking no longer serves `/api/wallet/mfe/` or `/wallet/mfe/`: a host on 0.3.0 or
  older loses the wallet until it upgrades.

## [0.4.0] - 2026-11-16

### Changed

- The wallet bundles moved to `https://evinvest.ltd/mfe/banking/`, the platform's
  `/mfe/<producer>/` layout: `WALLET_MFE_PREFIX`, `WALLET_REGISTRY_URL`,
  `walletMfePrefix`, the CSP sources and the integrity URLs follow. A host's CSP must
  name the new directory. Banking keeps serving `/api/wallet/mfe/` and `/wallet/mfe/`
  for hosts on older versions until they move.
- `walletCsp({ mode: 'inline' })` names concierge's new directory
  `https://evinvest.ltd/mfe/concierge/` beside its old `/api/auth/mfe/`.

## [0.3.0] - 2026-11-16

### Changed

- Renamed from `@evinvest/wallet` to `@evinvest/banking`, after its provider
  (as `@evinvest/concierge`); `@evinvest/wallet` is deprecated. Imports change
  their package name only: every export keeps its name.
- The wallet bundles moved to `https://evinvest.ltd/api/wallet/mfe/`, beside
  concierge's `/api/auth/mfe/`: `WALLET_MFE_PREFIX`, `WALLET_REGISTRY_URL`,
  `walletMfePrefix`, the CSP sources and the integrity URLs follow. A host's
  CSP must name the new directory. Banking keeps serving `/wallet/mfe/` for
  hosts on 0.2.0 until they move.

## [0.2.0] - 2026-10-28

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
- `walletCsp({ mode: 'inline' })` allows concierge's bundle directory
  (`https://evinvest.ltd/api/auth/mfe/`) in `script-src`, `style-src` and
  `connect-src`, for the step-up element that gives a money action its second
  factor.

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
