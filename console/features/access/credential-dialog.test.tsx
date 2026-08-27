import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { CredentialDialog } from './credential-dialog';
import { renderWithProviders } from '@/test/render';
import type { IssuedCredential } from '@/types/api';

const SECRET = 'record-store-secret-value-shown-once-1234';

const issued: IssuedCredential = {
  account: {
    id: 'acc-1',
    organization_id: 'org',
    name: 'backup-agent',
    description: '',
    disabled: false,
    created_at: '2026-08-01T10:00:00Z',
    updated_at: '2026-08-01T10:00:00Z',
  },
  credential: {
    id: 'cred-1',
    service_account_id: 'acc-1',
    key_id: 'RSKEYID000001',
    disabled: false,
    created_at: '2026-08-01T10:00:00Z',
    expires_at: null,
  },
  secret_access_key: SECRET,
};

describe('CredentialDialog', () => {
  it('masks the secret until the operator reveals it', async () => {
    renderWithProviders(
      <CredentialDialog issued={issued} onClose={vi.fn()} title="Created" description="d" />,
    );

    const value = screen.getByTestId('secret-value');
    expect(value.textContent).not.toContain(SECRET);
    expect(value.textContent).toMatch(/^•+$/);

    await userEvent.click(screen.getByRole('button', { name: /reveal/i }));
    expect(screen.getByTestId('secret-value').textContent).toBe(SECRET);

    await userEvent.click(screen.getByRole('button', { name: /hide/i }));
    expect(screen.getByTestId('secret-value').textContent).not.toContain(SECRET);
  });

  it('warns that the secret cannot be retrieved again', () => {
    renderWithProviders(
      <CredentialDialog issued={issued} onClose={vi.fn()} title="Created" description="d" />,
    );
    expect(screen.getByText(/will not be shown again/i)).toBeTruthy();
  });

  it('copies only on explicit action and never automatically', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } });

    renderWithProviders(
      <CredentialDialog issued={issued} onClose={vi.fn()} title="Created" description="d" />,
    );
    // Rendering the dialog must not touch the clipboard.
    expect(writeText).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole('button', { name: /copy access key ID/i }));
    expect(writeText).toHaveBeenCalledWith('RSKEYID000001');

    await userEvent.click(screen.getByRole('button', { name: /copy environment variables/i }));
    expect(writeText).toHaveBeenLastCalledWith(
      `AWS_ACCESS_KEY_ID=RSKEYID000001\nAWS_SECRET_ACCESS_KEY=${SECRET}`,
    );
    vi.unstubAllGlobals();
  });

  it('cannot be dismissed until the secret is acknowledged', async () => {
    const onClose = vi.fn();
    renderWithProviders(
      <CredentialDialog issued={issued} onClose={onClose} title="Created" description="d" />,
    );

    const done = screen.getByRole('button', { name: 'Done' });
    expect(done.hasAttribute('disabled')).toBe(true);

    await userEvent.click(screen.getByRole('checkbox'));
    expect(done.hasAttribute('disabled')).toBe(false);
    await userEvent.click(done);
    expect(onClose).toHaveBeenCalled();
  });

  it('renders nothing when there is no credential to show', () => {
    const { container } = renderWithProviders(
      <CredentialDialog issued={null} onClose={vi.fn()} title="Created" description="d" />,
    );
    expect(within(container).queryByTestId('secret-value')).toBeNull();
  });
});
