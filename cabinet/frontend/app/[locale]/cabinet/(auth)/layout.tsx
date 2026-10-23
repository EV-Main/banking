import type { ReactNode } from "react";

// Full-bleed wrapper for the unauthenticated surfaces. Each view owns its own framing —
// logged-out centers a card. No session parts here: this page is where a session ended.
export default function AuthLayout({ children }: { children: ReactNode }) {
  return <div className="min-h-[calc(100dvh-var(--ev-shell-offset,0px))] bg-background">{children}</div>;
}
