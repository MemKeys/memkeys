import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  SESSION_STORAGE_KEY,
  clearSession,
  hasSession,
  loadSession,
  saveSession,
} from './session';

// localStorage does not exist in the node test environment — stub it with a
// plain in-memory Map so the real storage code path is exercised.
const store = new Map<string, string>();
vi.stubGlobal('localStorage', {
  getItem: (k: string) => (store.has(k) ? store.get(k)! : null),
  setItem: (k: string, v: string) => void store.set(k, String(v)),
  removeItem: (k: string) => void store.delete(k),
  clear: () => store.clear(),
});

afterEach(() => store.clear());

describe('session persistence (ISI-6580)', () => {
  it('starts empty when nothing was stored', () => {
    expect(loadSession()).toEqual({ tenantId: '', token: '' });
    expect(hasSession(loadSession())).toBe(false);
  });

  it('round-trips tenant and token through localStorage', () => {
    saveSession({ tenantId: 'tenant-a', token: 'jwt-1' });
    expect(loadSession()).toEqual({ tenantId: 'tenant-a', token: 'jwt-1' });
    expect(hasSession(loadSession())).toBe(true);
  });

  it('a tenant without a token is still a session', () => {
    saveSession({ tenantId: 'loopback-dev' });
    const loaded = loadSession();
    expect(hasSession(loaded)).toBe(true);
    expect(loaded.token).toBe('');
  });

  it('clearSession removes the stored session', () => {
    saveSession({ tenantId: 'tenant-a', token: 'jwt-1' });
    clearSession();
    expect(loadSession()).toEqual({ tenantId: '', token: '' });
  });

  it('survives malformed stored JSON', () => {
    localStorage.setItem(SESSION_STORAGE_KEY, '{not json');
    expect(loadSession()).toEqual({ tenantId: '', token: '' });
  });

  it('ignores stored entries with non-string fields', () => {
    localStorage.setItem(SESSION_STORAGE_KEY, JSON.stringify({ tenantId: 42 }));
    expect(loadSession()).toEqual({ tenantId: '', token: '' });
  });

  it('hasSession treats a blank tenant as signed out', () => {
    expect(hasSession({ tenantId: '   ', token: 'jwt' })).toBe(false);
  });
});
