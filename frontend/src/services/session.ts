// Session persistence (ISI-6580): the signed-in tenant (+ optional bearer
// token) is stored in localStorage so a page refresh keeps the session, and a
// sign-out (or a mid-session API 401) removes it. Kept as pure functions with
// no React imports so the storage contract is unit-testable in node.
import { AuthConfig } from './api';

export const SESSION_STORAGE_KEY = 'memkeys-inspector-auth';

export type StoredSession = AuthConfig;

export function loadSession(): StoredSession {
  try {
    const raw = localStorage.getItem(SESSION_STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Partial<StoredSession>;
      return {
        tenantId: typeof parsed.tenantId === 'string' ? parsed.tenantId : '',
        token: typeof parsed.token === 'string' ? parsed.token : '',
      };
    }
  } catch {
    // malformed storage or localStorage unavailable — fall through to empty
  }
  return { tenantId: '', token: '' };
}

export function saveSession(session: StoredSession): void {
  try {
    localStorage.setItem(SESSION_STORAGE_KEY, JSON.stringify(session));
  } catch {
    // storage may be unavailable (private mode) — non-fatal
  }
}

export function clearSession(): void {
  try {
    localStorage.removeItem(SESSION_STORAGE_KEY);
  } catch {
    // non-fatal
  }
}

/** A session exists once a non-blank tenant id is present. */
export function hasSession(session: StoredSession): boolean {
  return session.tenantId.trim() !== '';
}
