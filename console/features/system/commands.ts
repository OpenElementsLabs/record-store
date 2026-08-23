import type { NavSection } from '@/features/system/navigation';
import type { RolePermissions } from '@/types/api';

/** One thing the palette can do. */
export type Command = {
  readonly id: string;
  readonly label: string;
  /** Groups commands in the list, and is searched along with the label. */
  readonly group: string;
  readonly href: string;
};

/**
 * Builds the palette's commands from the navigation the operator already has.
 *
 * Deriving from navigation rather than a second hand-written list means the
 * palette cannot offer a screen the sidebar hides — a role or a standalone
 * deployment gets the same answer from both.
 */
export function buildCommands(
  sections: readonly NavSection[],
  permissions: RolePermissions,
): readonly Command[] {
  const commands: Command[] = sections.flatMap((section) =>
    section.items.map((item) => ({
      id: `go:${item.href}`,
      label: item.label,
      group: section.title,
      href: item.href,
    })),
  );

  // Actions are query-string intents rather than palette-owned state, so the
  // destination screen decides what opening one means and the URL is shareable.
  if (permissions.manage_buckets) {
    commands.push({
      id: 'create:bucket',
      label: 'Create bucket',
      group: 'Actions',
      href: '/buckets?create=1',
    });
  }
  if (permissions.manage_service_accounts) {
    commands.push({
      id: 'create:service-account',
      label: 'Create service account',
      group: 'Actions',
      href: '/service-accounts?create=1',
    });
  }
  if (permissions.manage_policies) {
    commands.push({
      id: 'create:policy',
      label: 'Create policy',
      group: 'Actions',
      href: '/policies?create=1',
    });
  }
  return commands;
}

/**
 * Filters commands by a typed query.
 *
 * Matching is a subsequence test on the lowercased label and group, so "sacc"
 * finds "Service accounts" the way an operator expects from a palette, without
 * pulling in a fuzzy-search dependency.
 */
export function matchCommands(commands: readonly Command[], query: string): readonly Command[] {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) return commands;
  return commands.filter((command) =>
    isSubsequence(needle, `${command.group} ${command.label}`.toLowerCase()),
  );
}

function isSubsequence(needle: string, haystack: string): boolean {
  let index = 0;
  for (const character of haystack) {
    if (character === needle[index]) index += 1;
    if (index === needle.length) return true;
  }
  return index === needle.length;
}
