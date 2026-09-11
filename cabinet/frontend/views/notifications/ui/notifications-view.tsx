"use client";

import type { Locale, Translate } from "@evinvest/i18n";
import { useLocale, useT } from "@evinvest/i18n/react";

import { intlLocale } from "@/shared/lib/intl-locale";

import { Bell } from "lucide-react";
import { Link } from "@/shared/ui/cabinet-link";
import { useEffect, useState } from "react";

import { fetchNotifications } from "@/entities/notification/api/notification-client";
import { markRead, notificationsResource } from "@/entities/notification/model/notification-resource";
import { publishUnreadCount } from "@/entities/notification/model/notification-store";
import type { Notification } from "@/shared/contracts/notifications";
import { isUnread, toDate } from "@/shared/contracts/notifications";
import { errorMessage } from "@/shared/lib/api-client";
import { cn } from "@/shared/lib/cn";
import { useResource } from "@/shared/lib/resource";
import { SECTION_STAGGER, Stagger, StaggerItem } from "@/shared/ui/motion";

const CARD = "rounded-xl border border-border bg-card";
// Every control on this screen is hand-written rather than a uikit Button, so the keyboard
// focus ring has to be written out — once, here, so the four of them cannot drift apart.
const FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-ring";
// "Mark all read" and "Load older" are the same control in two places.
const GHOST_BUTTON = `rounded-lg border border-border/60 px-4 py-2 text-sm font-medium text-ink transition-colors hover:bg-ink/5 disabled:opacity-40 ${FOCUS}`;
// Rows are dense, so the inset is wider than the vertical rhythm.
const ROW_PAD = "px-5.5 py-4.5";
// A row spans the full width of a card that clips its overflow, so an outset ring would be
// shaved off on both sides — this one draws inside the row instead.
const ROW = "block outline-none transition-colors hover:bg-ink/5 focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring";

type Filter = "all" | "unread";

/**
 * The inbox. Reads are cursor-paginated and the filter round-trips to the server
 * rather than filtering client-side, so "Unread" is accurate across pages instead of
 * only within the ones already loaded.
 *
 * An empty list is ambiguous on its own — it means either "nothing has happened yet"
 * or "you switched the in-app channel off", and those want different copy. The
 * settings link in the empty state covers the second case without us having to fetch
 * settings here just to tell them apart.
 */
export function NotificationsView() {
  const t = useT();
  const locale = useLocale();
  const [filter, setFilter] = useState<Filter>("all");
  const [busy, setBusy] = useState(false);
  const [pageError, setPageError] = useState<string | null>(null);

  // The first page per filter is cached, so returning to the inbox shows the rows it last
  // showed instead of three skeleton bars. Later cursor pages are appended here and are not
  // cached: a page is only meaningful after the pages before it.
  const first = useResource(notificationsResource, filter);
  const page = first.data;
  const [older, setOlder] = useState<Notification[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);

  // A replaced first page — a filter switch, or a refresh after "mark all read" — voids the
  // pages appended under it. Reconciled during render so the two halves are never shown
  // stitched together for a frame.
  const [shown, setShown] = useState(page);
  if (shown !== page) {
    setShown(page);
    setOlder([]);
    setCursor(null);
  }

  const items = page ? [...page.notifications, ...older] : null;
  const nextCursor = cursor ?? page?.next_cursor ?? "";
  const unread = page?.unread_count ?? 0;
  const error = pageError ?? (page || !first.error ? null : errorMessage(first.error, t));

  // The badge in the rail reads the same count this page just learned, without its own poll.
  // In an effect, not during render: it writes to a store other components subscribe to.
  useEffect(() => {
    if (page) publishUnreadCount(page.unread_count);
  }, [page]);

  async function loadMore() {
    if (!nextCursor || busy) return;
    setBusy(true);
    try {
      const next = await fetchNotifications({ filter, cursor: nextCursor });
      setOlder((prev) => [...prev, ...next.notifications]);
      setCursor(next.next_cursor);
    } catch (e) {
      setPageError(errorMessage(e, t));
    } finally {
      setBusy(false);
    }
  }

  async function markAll() {
    if (busy || unread === 0) return;
    setBusy(true);
    try {
      // `markRead` invalidates both filters rather than patching: under "unread" the rows
      // that just changed should leave the list, which no local edit would do.
      await markRead();
    } catch (e) {
      setPageError(errorMessage(e, t));
    } finally {
      setBusy(false);
    }
  }

  async function open(n: Notification) {
    if (!isUnread(n) || !page) return;
    // Optimistic, written straight into the cache so the row stays read if the user leaves
    // and comes back. Reading is not a destructive act, and a failed mark simply shows up
    // again on the next refresh.
    notificationsResource.publish(
      {
        ...page,
        notifications: page.notifications.map((x) => (x.id === n.id ? { ...x, read_at: String(Math.floor(Date.now() / 1000)) } : x)),
        unread_count: Math.max(0, page.unread_count - 1),
      },
      filter,
    );
    setOlder((prev) => prev.map((x) => (x.id === n.id ? { ...x, read_at: String(Math.floor(Date.now() / 1000)) } : x)));
    try {
      await markRead([n.id]);
    } catch {
      /* the next refresh reconciles */
    }
  }

  return (
    <Stagger step={SECTION_STAGGER} className="mx-auto w-full max-w-282 px-4 py-6 sm:px-6 lg:px-8">
      <StaggerItem as="header" className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-3xl font-semibold text-ink">{t("nav.notifications")}</h1>
          <p className="mt-1 text-sm text-ink-soft">{t("notif.subtitle")}</p>
        </div>
        <button type="button" onClick={markAll} disabled={busy || unread === 0} className={GHOST_BUTTON}>
          {t("notif.markAllRead")}
        </button>
      </StaggerItem>

      {/* `inline-flex` on the bar itself, so the item that carries it has to stay inline
          too — a block wrapper would stretch the pill pair across the page. */}
      <StaggerItem className="mt-5 inline-flex gap-0.5 rounded-lg border border-border/60 bg-secondary p-1">
        {(["all", "unread"] as const).map((f) => (
          <button
            key={f}
            type="button"
            onClick={() => setFilter(f)}
            aria-pressed={filter === f}
            className={cn(
              "rounded-md px-3.5 py-1.5 text-sm transition-colors",
              FOCUS,
              filter === f ? "bg-primary font-semibold text-primary-foreground" : "font-medium text-ink hover:bg-ink/5",
            )}
          >
            {/* i18n-max: 14 — two pills in an `inline-flex` bar that cannot wrap. */}
            {f === "all" ? t("ui.all") : unread > 0 ? t("notif.unreadCount", { n: unread }) : t("notif.unread")}
          </button>
        ))}
      </StaggerItem>

      {error && (
        <StaggerItem as="p" className="mt-4 rounded-md border border-accent-error/40 bg-accent-error/10 px-3 py-2 text-sm text-accent-error">
          {error}
        </StaggerItem>
      )}

      <StaggerItem className={cn("mt-4 overflow-hidden", CARD)}>
        {items === null ? (
          <ul>
            {[0, 1, 2].map((i) => (
              <li key={i} className={cn("flex items-center gap-3.5", ROW_PAD, i > 0 && "border-t border-border/10")}>
                <div className="size-9 animate-pulse rounded-lg bg-ink/5" />
                <div className="flex-1 space-y-2">
                  <div className="h-3.5 w-1/3 animate-pulse rounded bg-ink/5" />
                  <div className="h-3 w-2/3 animate-pulse rounded bg-ink/5" />
                </div>
              </li>
            ))}
          </ul>
        ) : items.length === 0 ? (
          <EmptyState filter={filter} t={t} />
        ) : (
          <ul>
            {items.map((n, i) => (
              <Row key={n.id} n={n} first={i === 0} onOpen={() => void open(n)} locale={locale} t={t} />
            ))}
          </ul>
        )}
      </StaggerItem>

      {nextCursor && items && items.length > 0 && (
        <StaggerItem className="mt-4 flex justify-center">
          <button type="button" onClick={loadMore} disabled={busy} className={GHOST_BUTTON}>
            {busy ? t("ui.loading") : t("notif.loadOlder")}
          </button>
        </StaggerItem>
      )}
    </Stagger>
  );
}

function Row({ n, first, onOpen, locale, t }: { n: Notification; first: boolean; onOpen: () => void; locale: Locale; t: Translate }) {
  const unread = isUnread(n);
  const body = (
    <div className={cn("flex items-center gap-3.5 text-left", ROW_PAD, unread && "bg-ink/5")}>
      <span aria-hidden className={cn("size-2 shrink-0 rounded-full", unread ? "bg-accent-debug" : "bg-transparent")} />
      <div className="min-w-0 flex-1">
        {/* Read and unread titles share a step on the type scale, so the state is carried
            by weight and colour instead of the 1px that used to separate them. */}
        <p className={cn("truncate text-sm", unread ? "font-semibold text-ink" : "text-ink-soft")}>{n.title}</p>
        {n.body && <p className="mt-0.5 line-clamp-2 text-xs text-ink-soft">{n.body}</p>}
      </div>
      <time className="shrink-0 text-xs tabular-nums text-ink-soft" dateTime={toDate(n.created_at)?.toISOString()}>
        {formatWhen(n.created_at, locale, t)}
      </time>
    </div>
  );

  return (
    <li className={cn(!first && "border-t border-border/10")}>
      {n.link ? (
        <Link href={n.link as `/${string}`} onClick={onOpen} className={ROW}>
          {body}
        </Link>
      ) : (
        <button type="button" onClick={onOpen} className={cn("w-full", ROW)}>
          {body}
        </button>
      )}
    </li>
  );
}

function EmptyState({ filter, t }: { filter: Filter; t: Translate }) {
  return (
    <div className="flex flex-col items-center px-10 py-16 text-center">
      <span className="flex size-14 items-center justify-center rounded-xl bg-accent-debug/15">
        <Bell className="size-6 text-accent-debug" />
      </span>
      <p className="mt-5 text-base font-semibold text-ink">{t(filter === "unread" ? "notif.nothingUnread" : "notif.nothingYet")}</p>
      <p className="mt-2 max-w-108 text-sm text-ink-soft">{t(filter === "unread" ? "notif.allCaughtUp" : "notif.emptyHint")}</p>
      <Link
        href="/settings"
        className={cn("mt-5 rounded-lg border border-border px-5.5 py-2.5 text-sm font-semibold text-ink transition-colors hover:bg-ink/5", FOCUS)}
      >
        {t("notif.settingsLink")}
      </Link>
    </div>
  );
}

/** Relative for the first day, then a plain date — matching how the inbox is scanned.
 *  The date is formatted for the active locale, not the browser's: the cabinet's language
 *  comes from the URL, and someone reading in Russian on an English machine was getting
 *  "3 Sep" beside Russian copy. */
function formatWhen(unixSeconds: string, locale: Locale, t: Translate): string {
  const d = toDate(unixSeconds);
  if (!d) return "";
  const mins = Math.floor((Date.now() - d.getTime()) / 60_000);
  if (mins < 1) return t("notif.when.now");
  if (mins < 60) return t("notif.when.minutes", { n: mins });
  if (mins < 60 * 24) return t("notif.when.hours", { n: Math.floor(mins / 60) });
  if (mins < 60 * 24 * 7) return t("notif.when.days", { n: Math.floor(mins / (60 * 24)) });
  // `intlLocale`, not `locale`: bare "en" means en-US to Intl ("Mar 12"), while the
  // operations timeline renders "12 Mar". One English reader, one convention.
  return d.toLocaleDateString(intlLocale(locale), { day: "numeric", month: "short" });
}
