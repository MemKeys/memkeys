import React, { useEffect, useMemo, useState } from 'react';
import { AuthConfig } from './api';
import { ConfigContext } from './authContext';

// Connection config (tenant + optional bearer token) is held in one context so
// both tabs share it, and persisted to localStorage so a reload keeps it.
// The context object and `useAuth` hook live in ./authContext.

const STORAGE_KEY = 'memkeys-inspector-auth';

function load(): AuthConfig {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Partial<AuthConfig>;
      return { tenantId: parsed.tenantId ?? '', token: parsed.token ?? '' };
    }
  } catch {
    // ignore malformed storage
  }
  return { tenantId: '', token: '' };
}

export function ConfigProvider({ children }: { children: React.ReactNode }) {
  const [auth, setAuth] = useState<AuthConfig>(load);

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(auth));
    } catch {
      // storage may be unavailable (private mode) — non-fatal
    }
  }, [auth]);

  const value = useMemo(() => ({ auth, setAuth }), [auth]);
  return <ConfigContext.Provider value={value}>{children}</ConfigContext.Provider>;
}
