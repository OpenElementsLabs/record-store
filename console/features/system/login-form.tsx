'use client';

import { useRouter } from 'next/navigation';
import * as React from 'react';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Field } from '@/components/ui/label';

/**
 * Only relative paths are honoured after sign-in.
 *
 * Accepting an absolute URL here would turn the console into an open redirect.
 */
function safeRedirect(target: string): string {
  if (!target.startsWith('/') || target.startsWith('//')) return '/';
  return target;
}

export function LoginForm({ next }: { readonly next: string }) {
  const router = useRouter();
  const [token, setToken] = React.useState('');
  const [error, setError] = React.useState<string | null>(null);
  const [pending, setPending] = React.useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setError(null);
    setPending(true);
    try {
      const response = await fetch('/api/auth/login', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/json' },
        // The credential leaves the page in this request only. It is never
        // stored by script; the server replies with an HTTP-only cookie.
        body: JSON.stringify({ token }),
      });
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: { message?: string };
        } | null;
        setError(body?.error?.message ?? 'Sign-in failed.');
        return;
      }
      setToken('');
      router.replace(safeRedirect(next));
    } catch {
      setError('The console could not reach the OES management API.');
    } finally {
      setPending(false);
    }
  }

  return (
    <Card className="w-full max-w-md">
      <CardHeader className="flex-col items-start">
        <CardTitle className="text-base">Sign in to OES</CardTitle>
        <CardDescription>
          Enter a management token to administer this OES deployment.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} className="space-y-4">
          <Field
            label="Management token"
            htmlFor="management-token"
            hint="Configured on the server as a management role token."
            error={error}
          >
            <Input
              type="password"
              value={token}
              autoComplete="off"
              spellCheck={false}
              required
              onChange={(event) => setToken(event.target.value)}
            />
          </Field>
          <Button
            type="submit"
            variant="primary"
            className="w-full"
            disabled={pending || token.length === 0}
          >
            {pending ? 'Signing in…' : 'Sign in'}
          </Button>
        </form>
      </CardContent>
    </Card>
  );
}
