import '@testing-library/react';

import { afterEach, vi } from 'vitest';
import { cleanup } from '@testing-library/react';

// Node 24 exposes an optional global `localStorage` whose value is undefined
// unless the process receives a backing-file flag. That shadows jsdom's
// implementation in Vitest, so install the same small Storage contract a
// browser provides and clear it between tests.
const storedValues = new Map<string, string>();
const memoryStorage: Storage = {
  get length() {
    return storedValues.size;
  },
  clear: () => storedValues.clear(),
  getItem: (key) => storedValues.get(key) ?? null,
  key: (index) => [...storedValues.keys()][index] ?? null,
  removeItem: (key) => storedValues.delete(key),
  setItem: (key, value) => storedValues.set(key, String(value)),
};
Object.defineProperty(window, 'localStorage', {
  configurable: true,
  value: memoryStorage,
});

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  vi.restoreAllMocks();
});

// jsdom does not implement matchMedia, which Radix and the theme toggle read.
if (!window.matchMedia) {
  window.matchMedia = ((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
}

// Radix menus and dialogs measure elements that jsdom cannot lay out.
if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = () => {};
}
