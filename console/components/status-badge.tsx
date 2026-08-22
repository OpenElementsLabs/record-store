import {
  AlertTriangle,
  Ban,
  CheckCircle2,
  CircleDashed,
  CircleHelp,
  CircleSlash,
  Clock,
  PauseCircle,
  XCircle,
} from 'lucide-react';
import type * as React from 'react';

import { Badge } from '@/components/ui/badge';

/**
 * The semantic states the console renders.
 *
 * Every state maps to an icon and a label as well as a colour, so the meaning
 * survives for colour-blind users and in monochrome.
 */
export type StatusLevel =
  | 'healthy'
  | 'degraded'
  | 'warning'
  | 'critical'
  | 'unavailable'
  | 'pending'
  | 'paused'
  | 'disabled'
  | 'unknown';

const PRESENTATION: Record<
  StatusLevel,
  {
    icon: React.ComponentType<{ className?: string }>;
    tone: 'ok' | 'warn' | 'danger' | 'info' | 'neutral';
  }
> = {
  healthy: { icon: CheckCircle2, tone: 'ok' },
  degraded: { icon: AlertTriangle, tone: 'warn' },
  warning: { icon: AlertTriangle, tone: 'warn' },
  critical: { icon: XCircle, tone: 'danger' },
  unavailable: { icon: CircleSlash, tone: 'danger' },
  pending: { icon: Clock, tone: 'info' },
  paused: { icon: PauseCircle, tone: 'neutral' },
  disabled: { icon: Ban, tone: 'neutral' },
  unknown: { icon: CircleHelp, tone: 'neutral' },
};

export function StatusBadge({
  level,
  label,
  className,
}: {
  readonly level: StatusLevel;
  readonly label: string;
  readonly className?: string;
}) {
  const { icon: Icon, tone } = PRESENTATION[level];
  return (
    <Badge tone={tone} className={className}>
      <Icon aria-hidden />
      <span>{label}</span>
    </Badge>
  );
}

/** A neutral placeholder badge used while a status is still loading. */
export function StatusPending({ label = 'Checking' }: { readonly label?: string }) {
  return (
    <Badge tone="neutral">
      <CircleDashed aria-hidden className="animate-spin" />
      <span>{label}</span>
    </Badge>
  );
}
