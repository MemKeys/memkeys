import React, { useEffect, useMemo, useState } from 'react';
import { AuthConfig, onUnauthorized } from './api';
import { ConfigContext } from './authContext';
import { clearSession, hasSession, loadSession, saveSession } from './session';

// Connection config (tenant + optional bearer token) is held in one context so
// both tabs share it, and persisted to localStorage so a reload keeps it.
// Any API 401 clears the session (ISI-6580): the app returns to the login
// page with a "session expired" notice.
// The context object and `useAuth` hook live in ./authContext.

const EMPTY: AuthConfig = { tenantId: '', token: '' };

export function ConfigProvider({ children }: { children: React.ReactNode }) {
  const [auth, setAuth] = useState<AuthConfig>(loadSession);
  const [sessionExpired, setSessionExpired] = useState(false);

  useEffect(() => {
    if (hasSession(auth)) saveSession(auth);
    else clearSession();
  }, [auth]);

  useEffect(
    () =>
      onUnauthorized(() => {
        clearSession();
        setAuth(EMPTY);
        setSessionExpired(true);
      }),
    [],
  );

  const value = useMemo(
    () => ({
      auth,
      setAuth: (next: AuthConfig) => {
        setSessionExpired(false);
        setAuth(next);
      },
      sessionExpired,
      acknowledgeSessionExpired: () => setSessionExpired(false),
      signOut: () => {
        clearSession();
        setAuth(EMPTY);
        setSessionExpired(false);
      },
    }),
    [auth, sessionExpired],
  );

  return <ConfigContext.Provider value={value}>{children}</ConfigContext.Provider>;
}
