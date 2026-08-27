'use client';

import { Monitor, Moon, Sun } from 'lucide-react';
import * as React from 'react';

import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';

type Theme = 'light' | 'dark' | 'system';

const STORAGE_KEY = 'record-store-theme';
/** Notifies other instances in this tab, which `storage` events do not cover. */
const CHANGE_EVENT = 'record-store-theme-change';

function subscribe(onChange: () => void): () => void {
  window.addEventListener('storage', onChange);
  window.addEventListener(CHANGE_EVENT, onChange);
  return () => {
    window.removeEventListener('storage', onChange);
    window.removeEventListener(CHANGE_EVENT, onChange);
  };
}

function readStored(): Theme {
  const stored = window.localStorage.getItem(STORAGE_KEY);
  return stored === 'light' || stored === 'dark' ? stored : 'system';
}

/** The server has no preference to read, so it renders the system default. */
function serverSnapshot(): Theme {
  return 'system';
}

function apply(theme: Theme): void {
  const dark =
    theme === 'dark' ||
    (theme === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches);
  document.documentElement.classList.toggle('dark', dark);
}

/**
 * Switches between light, dark, and the operating-system preference.
 *
 * The stored preference is read with `useSyncExternalStore` rather than copied
 * into state by an effect, so the control always reflects what is actually
 * stored. Only the preference is persisted; nothing sensitive is.
 */
export function ThemeToggle() {
  const theme = React.useSyncExternalStore(subscribe, readStored, serverSnapshot);

  function choose(next: Theme) {
    if (next === 'system') window.localStorage.removeItem(STORAGE_KEY);
    else window.localStorage.setItem(STORAGE_KEY, next);
    apply(next);
    window.dispatchEvent(new Event(CHANGE_EVENT));
  }

  const Icon = theme === 'dark' ? Moon : theme === 'light' ? Sun : Monitor;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label="Change colour theme">
          <Icon aria-hidden />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent>
        <DropdownMenuItem onSelect={() => choose('light')}>
          <Sun aria-hidden /> Light
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => choose('dark')}>
          <Moon aria-hidden /> Dark
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => choose('system')}>
          <Monitor aria-hidden /> System
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
