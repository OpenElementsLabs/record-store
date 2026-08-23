import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

const ROOT = join(__dirname, '..', '..');
const CSS = readFileSync(join(ROOT, 'app', 'globals.css'), 'utf8');

function sourceFiles(): readonly string[] {
  const found: string[] = [];
  for (const dir of ['app', 'components', 'features', 'lib']) {
    walk(join(ROOT, dir));
  }
  function walk(path: string) {
    for (const entry of readdirSync(path)) {
      if (entry === 'node_modules') continue;
      const child = join(path, entry);
      if (statSync(child).isDirectory()) walk(child);
      // This file documents the offending pattern, so it cannot scan itself.
      else if (/\.tsx?$/.test(entry) && child !== __filename) found.push(child);
    }
  }
  return found;
}

describe('design tokens', () => {
  /*
   * Tailwind 3 read `rounded-[--radius-control]` as a custom property
   * reference. Tailwind 4 does not: it emits `border-radius: --radius-control`,
   * an invalid declaration the browser discards silently. That is how every
   * card and control in the console lost its rounded corners without a single
   * test, lint rule, or build step objecting.
   */
  it('uses no Tailwind 3 bare custom-property shorthand', () => {
    const offenders = sourceFiles()
      .map((file) => ({
        file,
        hits: readFileSync(file, 'utf8').match(/[a-z-]+-\[--[a-z0-9-]+\]/g),
      }))
      .filter((entry) => entry.hits)
      .map((entry) => `${entry.file.slice(ROOT.length + 1)}: ${entry.hits!.join(', ')}`);

    expect(offenders).toEqual([]);
  });

  /*
   * The sign-in screen was written against shadcn's token names while the theme
   * defined only the OES ones, so its card, muted text, and primary button
   * rendered with no colour at all. Both vocabularies must resolve, in both
   * themes — the dark block redefines the palette it aliases, so one set in
   * `:root` would leave dark mode pointing at light values.
   */
  const SHADCN_TOKENS = [
    'card',
    'card-foreground',
    'popover',
    'muted',
    'muted-foreground',
    'primary',
    'primary-foreground',
    'secondary',
    'secondary-foreground',
    'destructive',
    'input',
    'ring',
  ] as const;

  it.each(SHADCN_TOKENS)('defines --color-%s in both themes', (token) => {
    const definitions = CSS.match(new RegExp(`^\\s*--color-${token}:`, 'gm')) ?? [];
    expect(definitions).toHaveLength(2);
  });

  it('resolves every aliased token to a defined one', () => {
    const defined = new Set(Array.from(CSS.matchAll(/^\s*(--[a-z0-9-]+):/gm), (m) => m[1]));
    const referenced = Array.from(CSS.matchAll(/var\((--[a-z0-9-]+)\)/g), (m) => m[1]!);
    const dangling = [...new Set(referenced)].filter(
      (name) => !defined.has(name) && !name.startsWith('--font-geist'),
    );

    expect(dangling).toEqual([]);
  });

  it('names a radius for every level the system uses', () => {
    for (const radius of ['xs', 'inner', 'control', 'panel']) {
      expect(CSS).toContain(`--radius-${radius}:`);
    }
  });
});
