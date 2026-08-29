import Link from 'next/link';

import { BrandMark } from '@/components/brand-mark';
import { Button } from '@/components/ui/button';

export default function NotFound() {
  return (
    <main className="flex min-h-screen items-center justify-center px-4">
      <div className="max-w-md space-y-3 text-center">
        <BrandMark className="mx-auto mb-6 w-12" />
        <h1 className="text-lg font-semibold text-ink">Page not found</h1>
        <p className="text-sm text-ink-muted">
          That console route does not exist. It may have been renamed, or the feature may not be
          available in this deployment.
        </p>
        <Button asChild size="sm">
          <Link href="/">Back to overview</Link>
        </Button>
      </div>
    </main>
  );
}
