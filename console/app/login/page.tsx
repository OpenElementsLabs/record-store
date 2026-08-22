import type { Metadata } from 'next';

import { LoginForm } from '@/features/system/login-form';

export const metadata: Metadata = { title: 'Sign in' };

export default async function LoginPage({
  searchParams,
}: {
  searchParams: Promise<Record<string, string | string[] | undefined>>;
}) {
  const params = await searchParams;
  const next = typeof params.next === 'string' ? params.next : '/';
  return (
    <main className="flex min-h-screen items-center justify-center px-4 py-12">
      <LoginForm next={next} />
    </main>
  );
}
