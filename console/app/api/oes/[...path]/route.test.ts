import { describe, expect, it } from 'vitest';

import { DELETE, GET, HEAD, PATCH, POST, PUT } from './route';

describe('management proxy route methods', () => {
  it('exposes every method supported by the management API boundary', () => {
    expect(
      [GET, HEAD, POST, PUT, PATCH, DELETE].every((handler) => typeof handler === 'function'),
    ).toBe(true);
  });
});
