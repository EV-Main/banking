# Scope ledger listener TLS fixtures

Throwaway PEMs for `tests/scope_ledger_listener.rs`: `ca.pem` signs `server.pem` (SAN
`localhost`, `127.0.0.1`) and the client identities `invest.pem`, `panel.pem` and
`stranger.pem`; `foreign-ca.pem` signs `foreign.pem`, which carries the same name as
`invest` and is trusted by nothing. `<client>.sha256` is the SHA-256 of the client's DER
certificate — what the grants file maps to a client id. Nothing outside the test suite
trusts any of them, and the CA keys were not kept. Regenerate with `bash regen.sh` (needs
`openssl`); valid to 2036.
