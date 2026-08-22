import { describe, expect, it } from 'vitest';

import { validateBucketName } from './bucket-name';

describe('validateBucketName', () => {
  it('accepts names the backend accepts', () => {
    for (const name of ['uploads', 'my-bucket', 'data.archive', 'a1b']) {
      expect(validateBucketName(name)).toBeNull();
    }
  });

  it('rejects lengths outside the allowed range', () => {
    expect(validateBucketName('')).toContain('Enter');
    expect(validateBucketName('ab')).toContain('between 3 and 63');
    expect(validateBucketName('a'.repeat(64))).toContain('between 3 and 63');
  });

  it('rejects unsafe characters and shapes', () => {
    expect(validateBucketName('Uploads')).not.toBeNull();
    expect(validateBucketName('my_bucket')).not.toBeNull();
    expect(validateBucketName('-leading')).not.toBeNull();
    expect(validateBucketName('trailing-')).not.toBeNull();
    expect(validateBucketName('a..b')).toContain('adjacent');
    expect(validateBucketName('192.168.0.1')).toContain('IP address');
  });
});
