// TON friendly-address encoder (the TEP-2 user-friendly form) for displaying the raw
// `workchain:account-hex` form the hub stores. Pure TS, no dependency: 36 bytes
// (tag · workchain int8 · 32-byte account id · CRC16/XMODEM big-endian) base64url-encoded
// to exactly 48 chars. Non-bounceable (`UQ…`) is the DEFAULT — an uninitialized per-user
// deposit wallet would bounce funds sent to the bounceable `EQ…` form.

const RAW_RE = /^(-?\d+):([0-9a-fA-F]{64})$/;

// What to SHOW for an address the hub stored, whatever the rail. Only TON needs
// rewriting — its raw `workchain:hex` is valid but wallet-hostile and reads as an opaque
// hex blob rather than an address; every other rail already stores its user-facing form.
// Anything unconvertible (an already-friendly TON string, an EVM/TRON address) passes
// through untouched.
export function displayAddress(network: string, raw: string, opts?: { testnet?: boolean }): string {
  if (network !== "ton" || !raw) return raw;
  return tonFriendlyAddress(raw, { testnet: opts?.testnet }) ?? raw;
}

export function tonFriendlyAddress(raw: string, opts?: { bounceable?: boolean; testnet?: boolean }): string | null {
  const m = RAW_RE.exec(raw);
  if (!m) return null; // non-TON / already-friendly input — caller falls back to raw
  const workchain = Number(m[1]);
  if (workchain < -128 || workchain > 127) return null; // must fit the int8 slot

  const bytes = new Uint8Array(36);
  // Base tag: bounceable 0x11 / non-bounceable 0x51. TEP-2's test-only flag sets bit 0x80
  // (→ 0x91 bounceable-test / 0xD1 non-bounceable-test); the CRC below is over the tagged bytes,
  // so a testnet address serializes to a different checksummed string than its mainnet twin.
  let tag = opts?.bounceable ? 0x11 : 0x51;
  if (opts?.testnet) tag |= 0x80;
  bytes[0] = tag;
  bytes[1] = workchain & 0xff; // int8: 0 → 0x00, -1 → 0xff
  for (let i = 0; i < 32; i++) bytes[2 + i] = parseInt(m[2]!.slice(i * 2, i * 2 + 2), 16);
  const crc = crc16Xmodem(bytes.subarray(0, 34));
  bytes[34] = crc >> 8;
  bytes[35] = crc & 0xff;
  return base64Url(bytes);
}

// CRC16/XMODEM (poly 0x1021, init 0x0000) — the checksum TEP-2 addresses carry.
function crc16Xmodem(bytes: Uint8Array): number {
  let crc = 0;
  for (const byte of bytes) {
    crc ^= byte << 8;
    for (let i = 0; i < 8; i++) crc = crc & 0x8000 ? ((crc << 1) ^ 0x1021) & 0xffff : (crc << 1) & 0xffff;
  }
  return crc;
}

function base64Url(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  // 36 bytes divide evenly into base64 (48 chars) — no `=` padding to strip.
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_");
}
