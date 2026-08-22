'use client';

import { Check, Copy } from 'lucide-react';
import * as React from 'react';

import { Button, type ButtonProps } from '@/components/ui/button';

/**
 * Copies a value to the clipboard on explicit user action.
 *
 * Nothing is ever copied automatically, and the value itself is never logged.
 */
export function CopyButton({
  value,
  label,
  size = 'sm',
  variant = 'secondary',
  className,
}: {
  readonly value: string;
  readonly label: string;
  readonly size?: ButtonProps['size'];
  readonly variant?: ButtonProps['variant'];
  readonly className?: string;
}) {
  const [copied, setCopied] = React.useState(false);
  const timer = React.useRef<ReturnType<typeof setTimeout> | null>(null);

  React.useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      if (timer.current) clearTimeout(timer.current);
      timer.current = setTimeout(() => setCopied(false), 1500);
    } catch {
      // A clipboard denied by the browser is not an application failure; the
      // value stays selectable on screen.
      setCopied(false);
    }
  }

  return (
    <Button
      size={size}
      variant={variant}
      className={className}
      onClick={copy}
      aria-label={copied ? `${label} copied` : `Copy ${label}`}
    >
      {copied ? <Check aria-hidden /> : <Copy aria-hidden />}
      <span>{copied ? 'Copied' : 'Copy'}</span>
    </Button>
  );
}
