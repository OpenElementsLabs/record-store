'use client';

import { ArrowRight, Eye, EyeOff, LockKeyhole, ShieldCheck } from 'lucide-react';
import { useRouter } from 'next/navigation';
import * as React from 'react';

import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';

function safeRedirect(target: string) {
  if (!target.startsWith('/') || target.startsWith('//')) return '/';
  return target;
}

export function LoginForm({ next }: { readonly next: string }) {
  const router = useRouter();
  const [token, setToken] = React.useState('');
  const [showToken, setShowToken] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [pending, setPending] = React.useState(false);

  async function submit(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setError(null);
    setPending(true);
    try {
      const response = await fetch('/api/auth/login', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ token }),
      });
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: { message?: string };
        } | null;
        setError(body?.error?.message ?? 'Sign-in failed. Check your token and try again.');
        return;
      }
      // Cleared before navigating so the token does not sit in component state
      // for the lifetime of the session.
      setToken('');
      router.replace(safeRedirect(next));
    } catch {
      setError('The console could not reach the Record Store management API.');
    } finally {
      setPending(false);
    }
  }

  return (
    <section className="w-full max-w-[420px]">
      <div className="mb-9">
        <p className="mb-4 flex items-center gap-3 font-mono text-xs font-medium uppercase tracking-[0.18em] text-brand-teal">
          <span aria-hidden className="size-2 rotate-45 bg-brand-gold" />
          Secure access
        </p>
        <h1 className="type-display">Welcome back</h1>
        <p className="mt-3 max-w-sm text-sm leading-6 text-foreground-muted">
          Enter the management token for this Record Store deployment.
        </p>
      </div>

      <form onSubmit={submit} className="space-y-7">
        <div className="space-y-2.5">
          <label htmlFor="management-token" className="text-sm font-medium text-foreground">
            Management token
          </label>
          <div className="relative">
            <LockKeyhole
              aria-hidden="true"
              className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-foreground-muted"
            />
            <Input
              id="management-token"
              type={showToken ? 'text' : 'password'}
              value={token}
              autoComplete="off"
              spellCheck={false}
              required
              aria-invalid={error ? true : undefined}
              aria-describedby={error ? 'management-token-error' : 'management-token-hint'}
              onChange={(event) => setToken(event.target.value)}
              className="h-13 border-border-strong bg-surface pl-10 pr-11 shadow-sm focus-visible:border-brand-teal focus-visible:ring-brand-teal"
              placeholder="Enter your token"
            />
            <Button
              variant="ghost"
              size="icon"
              className="absolute right-1 top-1"
              onClick={() => setShowToken((visible) => !visible)}
              aria-label={showToken ? 'Hide token' : 'Show token'}
            >
              {showToken ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
            </Button>
          </div>
          {error ? (
            <p id="management-token-error" role="alert" className="text-xs text-danger">
              {error}
            </p>
          ) : (
            <p id="management-token-hint" className="text-xs leading-5 text-foreground-muted">
              Configured on the server as a management role token.
            </p>
          )}
        </div>

        <Button
          type="submit"
          variant="primary"
          size="lg"
          disabled={pending || token.length === 0}
          className="group h-13 w-full justify-between bg-brand-navy text-white shadow-md shadow-brand-navy/15 hover:bg-brand-navy-deep dark:bg-brand-teal dark:text-brand-navy-deep dark:hover:bg-brand-teal/90"
        >
          <span>{pending ? 'Signing in…' : 'Continue to console'}</span>
          <ArrowRight aria-hidden className="transition-transform group-hover:translate-x-1" />
        </Button>
      </form>

      <div className="mt-9 flex gap-3 rounded-xl border border-brand-teal/20 bg-brand-teal/5 p-4">
        <ShieldCheck aria-hidden className="mt-0.5 size-4 shrink-0 text-brand-teal" />
        <div>
          <p className="text-xs font-semibold text-foreground">Protected sign-in</p>
          <p className="mt-1 text-xs leading-5 text-foreground-muted">
            Your token is sent over an encrypted connection and never stored in this browser.
          </p>
        </div>
      </div>
    </section>
  );
}
