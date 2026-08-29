import type { Metadata } from 'next';
import { Activity, Database, ShieldCheck } from 'lucide-react';

import { BrandLockup, BrandMark } from '@/components/brand-mark';
import { ThemeToggle } from '@/components/theme-toggle';
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
    <main className="min-h-dvh lg:grid lg:grid-cols-[minmax(0,1.1fr)_minmax(28rem,0.9fr)]">
      <aside
        aria-label="About Record Store"
        className="relative hidden min-h-dvh overflow-hidden bg-brand-navy px-10 py-9 text-white lg:flex lg:flex-col xl:px-16 xl:py-12"
      >
        <div
          aria-hidden="true"
          className="absolute inset-0 bg-[radial-gradient(circle_at_82%_14%,rgba(11,179,184,0.34),transparent_31%),radial-gradient(circle_at_12%_92%,rgba(255,187,61,0.2),transparent_28%)]"
        />
        <BrandMark className="absolute -bottom-28 -right-20 w-[31rem] opacity-[0.075]" />

        <BrandLockup className="relative z-10" size="default" tone="inverse" />

        <div className="relative z-10 my-auto max-w-2xl py-16">
          <p className="mb-6 flex items-center gap-3 font-mono text-xs font-medium uppercase tracking-[0.2em] text-white/70">
            <span aria-hidden className="size-2 rotate-45 bg-brand-gold" />
            Record Store Console
          </p>
          <h1 className="max-w-xl text-4xl font-semibold leading-[1.08] tracking-[-0.045em] text-balance xl:text-5xl">
            Object storage, clearly under control.
          </h1>
          <p className="mt-6 max-w-xl text-base leading-7 text-white/72 xl:text-lg xl:leading-8">
            Monitor health, verify integrity, and govern access from one focused operational
            console.
          </p>

          <ul className="mt-10 grid max-w-2xl grid-cols-3 gap-3" aria-label="Console capabilities">
            <BrandCapability icon={Activity} label="Health" detail="See deployment status" />
            <BrandCapability icon={Database} label="Integrity" detail="Verify stored objects" />
            <BrandCapability icon={ShieldCheck} label="Access" detail="Govern every action" />
          </ul>
        </div>

        <p className="relative z-10 text-xs leading-5 text-white/55">
          Administrative access to this deployment is protected by your management token.
        </p>
      </aside>

      <section className="flex min-h-dvh flex-col bg-canvas px-5 py-5 sm:px-10 sm:py-7 lg:px-12 xl:px-20">
        <header className="flex items-center justify-between">
          <BrandLockup className="lg:hidden" size="compact" />
          <div className="ml-auto">
            <ThemeToggle />
          </div>
        </header>

        <div className="flex flex-1 items-center justify-center py-10 sm:py-14">
          <LoginForm next={next} />
        </div>

        <footer className="text-center text-xs leading-5 text-foreground-subtle">
          Record Store management console
        </footer>
      </section>
    </main>
  );
}

function BrandCapability({
  icon: Icon,
  label,
  detail,
}: {
  readonly icon: React.ComponentType<{ className?: string; 'aria-hidden'?: boolean }>;
  readonly label: string;
  readonly detail: string;
}) {
  return (
    <li className="rounded-xl border border-white/12 bg-white/[0.055] p-4 backdrop-blur-sm">
      <Icon aria-hidden className="size-4 text-brand-teal" />
      <p className="mt-4 text-sm font-semibold text-white">{label}</p>
      <p className="mt-1 text-xs leading-5 text-white/60">{detail}</p>
    </li>
  );
}
