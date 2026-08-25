'use client';

import { Ban, CheckCircle2, Clock, Globe, History, KeyRound, Pin, XCircle } from 'lucide-react';
import type * as React from 'react';

import { Badge } from '@/components/ui/badge';
import { formatDateTime, formatRelativeTime } from '@/lib/format';
import type { CapabilityStatus, VersionMode } from '@/types/api';

/**
 * How a capability's state is shown.
 *
 * Every state carries an icon and a word as well as a colour, because an
 * operator deciding whether a link is still live must not have to distinguish
 * green from amber to find out.
 */
const STATUS_PRESENTATION: Record<
  CapabilityStatus,
  {
    readonly icon: React.ComponentType<{ className?: string; 'aria-hidden'?: boolean }>;
    readonly tone: 'ok' | 'warn' | 'danger' | 'neutral';
    readonly label: string;
  }
> = {
  active: { icon: CheckCircle2, tone: 'ok', label: 'Active' },
  revoked: { icon: Ban, tone: 'danger', label: 'Revoked' },
  expired: { icon: Clock, tone: 'neutral', label: 'Expired' },
  exhausted: { icon: XCircle, tone: 'warn', label: 'Limit reached' },
};

export function CapabilityStatusBadge({ status }: { readonly status: CapabilityStatus }) {
  const { icon: Icon, tone, label } = STATUS_PRESENTATION[status];
  return (
    <Badge tone={tone}>
      <Icon aria-hidden />
      <span>{label}</span>
    </Badge>
  );
}

/** Says which version a capability resolves to, and never leaves it implicit. */
export function VersionModeBadge({ mode }: { readonly mode: VersionMode }) {
  return mode === 'pinned' ? (
    <Badge tone="info">
      <Pin aria-hidden />
      <span>Pinned version</span>
    </Badge>
  ) : (
    <Badge tone="neutral">
      <History aria-hidden />
      <span>Current version</span>
    </Badge>
  );
}

export function PasswordBadge() {
  return (
    <Badge tone="accent">
      <KeyRound aria-hidden />
      <span>Password protected</span>
    </Badge>
  );
}

export function OriginBadge({ count }: { readonly count: number }) {
  return (
    <Badge tone={count > 0 ? 'accent' : 'warn'}>
      <Globe aria-hidden />
      <span>{count > 0 ? `${count} allowed origin${count === 1 ? '' : 's'}` : 'Any origin'}</span>
    </Badge>
  );
}

/**
 * Renders an expiry as an absolute date with a relative hint.
 *
 * Both, because "in 3 days" is what a reader wants to know and the exact
 * timestamp is what they need when it matters.
 */
export function ExpiryLabel({ expiresAt }: { readonly expiresAt: string | null }) {
  if (!expiresAt) {
    return (
      <span className="type-meta">
        <span className="text-warn">Never expires</span>
      </span>
    );
  }
  return (
    <span className="type-meta">
      Expires <time dateTime={expiresAt}>{formatDateTime(expiresAt)}</time> (
      {formatRelativeTime(expiresAt)})
    </span>
  );
}
