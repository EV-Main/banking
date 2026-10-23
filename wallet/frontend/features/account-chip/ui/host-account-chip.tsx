"use client";

import { displayName, initialsOfName, truncateName } from "@/shared/lib/identity";

import { chipTranslator } from "../lib/chip-translator";
import type { HostChipProps } from "../lib/host-attributes";
import { AccountMenu, type MenuIdentity } from "./account-menu";

/** Same cap as the wallet host chip, so both hosts lay the label out alike. */
const MAX_LABEL = 28;

export type HostAccountChipProps = HostChipProps & {
  className?: string;
  /** The element turns this into its `signout` event; the host owns the session. */
  onSignOut: () => void;
};

/**
 * The chip on a host with its own session (`data-mode="host"`): everything it shows comes
 * from the element's attributes. No session read, no profile read, no identity cache, no
 * logout request — the host holds all of those, and this bundle cannot see its cookies.
 *
 * Neither name nor email yet is the host still loading: the skeleton, in the final box.
 */
export function HostAccountChip({ className, name, email, manageHref, profileHref, switchHref, groups, onSignOut }: HostAccountChipProps) {
  const identity: MenuIdentity | null =
    name || email
      ? {
          email,
          initials: initialsOfName(name ?? "", email),
          label: truncateName(name ?? displayName(email, chipTranslator()), MAX_LABEL),
        }
      : null;
  return (
    <AccountMenu
      className={className}
      identity={identity}
      links={{ profile: profileHref ? { href: profileHref, kind: "cabinet" } : null, manage: manageHref, switchAccount: switchHref }}
      groups={groups}
      onSignOut={onSignOut}
    />
  );
}
