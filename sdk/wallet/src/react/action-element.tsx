import { RemoteElement, type AttributeValue } from '@evinvest/mfe/react';
import type { ReactNode } from 'react';

import { INLINE_ACTIONS_ATTRIBUTE, WALLET_API_VERSION, type WalletActionMode, type WalletActionOutcome } from '../index';
import { actionEvents, inlineActions } from './listeners';

// What every wallet button shares. The public props interfaces keep their own copies, for
// their per-button doc comments; the defaults live here only.
interface ActionButtonProps {
  scriptUrl: string;
  scope?: string;
  mode?: WalletActionMode;
  onResult: (outcome: WalletActionOutcome) => void;
  origin?: string;
  minApiVersion?: number;
  className?: string;
  fallback?: ReactNode;
}

interface ActionElementProps extends ActionButtonProps {
  tag: string;
  /** The element's own id attribute, e.g. `{ invoice }` or `{ request }`. */
  target: Readonly<Record<string, AttributeValue>>;
}

/** Internal: the one element for every wallet button. Not exported from the package. */
export function ActionElement({
  tag,
  target,
  scriptUrl,
  scope,
  mode = 'popup',
  onResult,
  origin,
  minApiVersion = WALLET_API_VERSION,
  className,
  fallback = null,
}: ActionElementProps) {
  return (
    <RemoteElement
      tag={tag}
      scriptUrl={scriptUrl}
      minApiVersion={minApiVersion}
      {...(className === undefined ? {} : { className })}
      attributes={{ ...target, scope, [INLINE_ACTIONS_ATTRIBUTE]: inlineActions(mode) }}
      events={actionEvents({ onResult, origin, mode })}
      fallback={fallback}
    />
  );
}
