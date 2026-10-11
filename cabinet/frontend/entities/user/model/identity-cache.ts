// Last authenticated identity, persisted per-tab so a return visit (a hard load re-mounts
// the account-chip remote from scratch) paints the final chip on the first frame instead of
// cycling skeleton → email-derived name → real name. Only the two bits the chip renders are
// stored — both are the user's own, already-visible identity — never the full profile PII.
// sessionStorage (not local) so it dies with the tab and revalidates each session. The
// paired `email` lets a consumer reject a seed that belongs to a different account (a
// same-tab user switch), so a stale name is never shown or re-persisted.

export type ChipIdentity = { email: string | null; name: string };

const IDENTITY_KEY = "ev.cabinet.account-chip";

export function readIdentity(): ChipIdentity | null {
  try {
    const raw = sessionStorage.getItem(IDENTITY_KEY);
    return raw ? (JSON.parse(raw) as ChipIdentity) : null;
  } catch {
    return null;
  }
}

export function writeIdentity(identity: ChipIdentity) {
  try {
    sessionStorage.setItem(IDENTITY_KEY, JSON.stringify(identity));
  } catch {
    // sessionStorage unavailable (private mode / disabled) — degrade to the live fetch
    // path; no cache, but correctness is unaffected.
  }
}

export function clearIdentity() {
  try {
    sessionStorage.removeItem(IDENTITY_KEY);
  } catch {
    /* nothing to clear */
  }
}
