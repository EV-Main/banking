"use client";

import { useRouter } from "next/navigation";
import { useEffect } from "react";

import { NAVIGATE_EVENT } from "@/shared/config/chip-element";

import { zoneRouteFor, fromChip } from "./chip-route";

/**
 * Takes the account chip's menu links that lead to zone pages and routes them client-side,
 * like a `<Link>` would, instead of the full load an `<a>` in the chip's own React root gets.
 * The chip comes into wallet host documents through the conductor's shell fragment, so it shares
 * this document and its events reach `document`. Renders nothing.
 *
 * Nothing from the chip's slice is imported: its index is the chip bundle's entry, and this
 * runs on every zone page, where the chip UI has no business in the client bundle.
 */
export function ChipNavigation() {
  const router = useRouter();
  useEffect(() => {
    const onNavigate = (event: Event) => {
      if (!(event instanceof CustomEvent) || !fromChip(event.composedPath())) return;
      const href = zoneRouteFor(event.detail);
      if (href === null) return;
      event.preventDefault();
      router.push(href);
    };
    document.addEventListener(NAVIGATE_EVENT, onNavigate);
    return () => document.removeEventListener(NAVIGATE_EVENT, onNavigate);
  }, [router]);
  return null;
}
