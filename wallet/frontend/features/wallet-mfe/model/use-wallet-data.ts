import { useCallback, useEffect, useRef, useState } from "react";

import { fetchBalance, fetchHistory } from "../api/wallet-remote";
import type { WalletBalance, WalletHistory } from "../lib/parse";

export type WalletData =
  | { status: "loading" }
  | { status: "signed_out" }
  | { status: "error" }
  | { status: "ready"; balance: WalletBalance | null; history: WalletHistory | null; refreshFailed: boolean };

/**
 * Balance and history from the wallet origin. A failed refresh keeps the figures already on
 * screen and flags them, rather than blanking a balance the reader was looking at.
 *
 * Re-reads when the host window regains focus: a money action runs in a popup on
 * evinvest.ltd, and closing it hands focus back here — the moment the figures may have moved.
 */
export function useWalletData(origin: string): { data: WalletData; reload: () => void } {
  const [data, setData] = useState<WalletData>({ status: "loading" });
  const generation = useRef(0);

  const reload = useCallback(() => {
    const mine = ++generation.current;
    void Promise.all([fetchBalance(origin), fetchHistory(origin)]).then(([balance, history]) => {
      // A slower, older read must not overwrite a newer one.
      if (mine !== generation.current) return;
      if ((!balance.ok && balance.reason === "signed_out") || (!history.ok && history.reason === "signed_out")) {
        setData({ status: "signed_out" });
        return;
      }
      setData((previous) => {
        if (!balance.ok && !history.ok) {
          return previous.status === "ready" ? { ...previous, refreshFailed: true } : { status: "error" };
        }
        const kept = previous.status === "ready" ? previous : null;
        return {
          status: "ready",
          balance: balance.ok ? balance.value : (kept?.balance ?? null),
          history: history.ok ? history.value : (kept?.history ?? null),
          refreshFailed: !balance.ok || !history.ok,
        };
      });
    });
  }, [origin]);

  useEffect(() => {
    const counter = generation;
    reload();
    window.addEventListener("focus", reload);
    return () => {
      window.removeEventListener("focus", reload);
      // Drops the answer of a read still in flight when the element goes away. The ref
      // object is captured above, so the cleanup bumps the live counter, not a copy.
      counter.current++;
    };
  }, [reload]);

  return { data, reload };
}
