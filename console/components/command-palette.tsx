'use client';

import { Search } from 'lucide-react';
import { useRouter } from 'next/navigation';
import * as React from 'react';

import { Dialog, DialogContent, DialogTitle } from '@/components/ui/dialog';
import { buildCommands, matchCommands, type Command } from '@/features/system/commands';
import { usePermissions } from '@/features/system/deployment';
import type { NavSection } from '@/features/system/navigation';
import { cn } from '@/lib/utils';

/**
 * Keyboard-first navigation.
 *
 * The palette is a faster route to screens the operator can already reach, not
 * a second permission model: its commands come from the same navigation the
 * sidebar renders.
 */
export function CommandPalette({
  sections,
  open,
  onOpenChange,
}: {
  readonly sections: readonly NavSection[];
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
}) {
  const permissions = usePermissions();

  const commands = React.useMemo(
    () => buildCommands(sections, permissions),
    [sections, permissions],
  );

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="p-0">
        {/* Mounted only while open, so the query and selection start fresh. */}
        <PaletteBody commands={commands} onClose={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  );
}

/**
 * Whether this browser uses the Command key.
 *
 * Read after hydration through an external store with a non-Mac server
 * snapshot, so the markup the server produced and the markup the client
 * produces agree on the first paint and the label corrects itself afterwards.
 */
export function useCommandKey(): boolean {
  return React.useSyncExternalStore(
    () => () => {},
    () => /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent),
    () => false,
  );
}

/**
 * The visible way in to the palette.
 *
 * A keyboard shortcut nobody is told about is not a feature, so the shortcut is
 * printed on the control that opens it.
 */
export function CommandTrigger({ onOpen }: { readonly onOpen: () => void }) {
  const command = useCommandKey();
  return (
    <button
      type="button"
      onClick={onOpen}
      className="flex h-8 items-center gap-2 rounded-[--radius-control] border border-border bg-surface-muted px-2.5 text-xs text-ink-muted hover:border-border-strong hover:text-ink focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
    >
      <Search aria-hidden className="size-3.5" />
      <span>Search</span>
      <kbd className="ml-2 hidden rounded border border-border-strong px-1 py-px font-sans text-[0.6875rem] text-ink-subtle sm:inline">
        {command ? '⌘' : 'Ctrl '}K
      </kbd>
    </button>
  );
}

function PaletteBody({
  commands,
  onClose,
}: {
  readonly commands: readonly Command[];
  readonly onClose: () => void;
}) {
  const router = useRouter();
  const [query, setQuery] = React.useState('');
  const [highlighted, setHighlighted] = React.useState(0);

  const matches = matchCommands(commands, query);
  // Clamp rather than reset: the highlight follows the list as it narrows.
  const active = Math.min(highlighted, Math.max(0, matches.length - 1));

  function run(command: Command | undefined) {
    if (!command) return;
    onClose();
    router.push(command.href);
  }

  return (
    <div className="flex flex-col">
      <DialogTitle className="sr-only">Command palette</DialogTitle>
      <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <Search aria-hidden className="size-4 shrink-0 text-ink-subtle" />
        <input
          autoFocus
          type="text"
          value={query}
          aria-label="Search commands"
          aria-controls="command-results"
          placeholder="Jump to a screen or action…"
          className="w-full bg-transparent text-sm text-ink outline-none placeholder:text-ink-subtle"
          onChange={(event) => {
            setQuery(event.target.value);
            setHighlighted(0);
          }}
          onKeyDown={(event) => {
            if (event.key === 'ArrowDown') {
              event.preventDefault();
              setHighlighted((current) => Math.min(current + 1, matches.length - 1));
            } else if (event.key === 'ArrowUp') {
              event.preventDefault();
              setHighlighted((current) => Math.max(current - 1, 0));
            } else if (event.key === 'Enter') {
              event.preventDefault();
              run(matches[active]);
            }
          }}
        />
      </div>

      <ul id="command-results" className="max-h-80 overflow-y-auto py-1" role="listbox">
        {matches.length === 0 ? (
          <li className="px-3 py-6 text-center text-sm text-ink-muted">
            Nothing matches “{query}”.
          </li>
        ) : (
          matches.map((command, index) => (
            <li key={command.id}>
              <button
                type="button"
                role="option"
                aria-selected={index === active}
                className={cn(
                  'flex w-full items-baseline gap-2 px-3 py-2 text-left text-sm',
                  index === active ? 'bg-surface-muted text-ink' : 'text-ink-muted',
                )}
                onMouseEnter={() => setHighlighted(index)}
                onClick={() => run(command)}
              >
                <span className="text-ink">{command.label}</span>
                <span className="text-xs text-ink-subtle">{command.group}</span>
              </button>
            </li>
          ))
        )}
      </ul>
    </div>
  );
}
