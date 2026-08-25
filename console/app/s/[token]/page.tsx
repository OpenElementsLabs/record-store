import type { Metadata } from 'next';
import { cookies } from 'next/headers';

import { ShareViewer } from '@/features/sharing/share-viewer';
import { isCapabilityToken } from '@/lib/capability-token';
import { readShareDescriptor } from '@/lib/server/capability-proxy';
import { SHARE_TICKET_COOKIE } from '@/lib/server/config';
import type { PublicShare } from '@/types/api';

type Params = { params: Promise<{ token: string }> };

/**
 * The page a share recipient opens.
 *
 * Nothing about the object reaches this page until the backend has authorized
 * the token, and nothing about OES reaches it at all: no bucket, no key path, no
 * version identifier, no node, no administrative navigation. A recipient needs
 * to recognise the file and decide whether to open it, and that is the whole of
 * what is disclosed.
 */
export const metadata: Metadata = {
  // A shared link must not turn into a search result, and its title must not
  // leak the file name to anything that renders a link preview.
  title: 'Shared file',
  robots: { index: false, follow: false, nocache: true },
};

export const dynamic = 'force-dynamic';

export default async function Page({ params }: Params) {
  const { token } = await params;

  // A link that no longer works is answered by the share page itself, never by
  // the console's own not-found page. Two reasons: that page carries
  // administrative navigation a recipient has no business seeing, and its
  // wording would be about a missing console route rather than about the link
  // they were sent. A malformed token takes the same path — someone who mistyped
  // a link should be told the link does not work, not dropped into an admin app.
  const usable = isCapabilityToken(token);

  // A ticket from an earlier unlock survives a reload, so a recipient who has
  // already entered the password is not asked for it again on every visit.
  const ticket = usable ? ((await cookies()).get(SHARE_TICKET_COOKIE)?.value ?? null) : null;
  const { status, body } = usable
    ? await readShareDescriptor(token, ticket)
    : { status: 404, body: null };

  // Expired, revoked, exhausted, and never-existed all arrive here as the same
  // answer, and the viewer renders them as the same message. Distinguishing them
  // would confirm a guess.
  const descriptor =
    status === 200 && body && typeof body === 'object' ? (body as PublicShare) : null;

  return <ShareViewer token={token} initial={descriptor} />;
}
