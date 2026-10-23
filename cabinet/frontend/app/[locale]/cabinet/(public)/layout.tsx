import type { ReactNode } from "react";

// The chromeless group for pages a signed-OUT visitor is expected to reach — today, the two
// token-addressed approval pages an owner opens from their mailbox.
//
// It is a sibling of `(auth)` rather than a page inside it because the two groups differ in
// what they must NOT do. `(auth)` holds sign-in and sign-out, which are about a session;
// these pages are about a token, and the reader may have no account on this device at all.
// So nothing session-dependent mounts here: no `SessionKeeper` (it exists to rotate an
// access cookie that does not exist, and its answer to "signed out" is a redirect to
// /login — which would throw the reader off a page they were mailed a link to), and no
// `CacheWarmer` (it warms authenticated reads). The URL's locale is the whole answer here.
//
// What is kept is `--ev-shell-offset` — the one contract with the conductor's header, which
// is injected around this zone whether or not anyone is signed in.
export default function PublicLayout({ children }: { children: ReactNode }) {
  return <div className="min-h-[calc(100dvh-var(--ev-shell-offset,0px))] bg-background">{children}</div>;
}
