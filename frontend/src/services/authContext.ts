import { createContext, useContext } from 'react';
import { AuthConfig } from './api';

// Context + hook live apart from the provider component so the provider file
// exports only a component (keeps react-refresh / fast-refresh happy).

export interface ConfigContextValue {
  auth: AuthConfig;
  setAuth: (next: AuthConfig) => void;
  /** True right after a mid-session API 401 forced the user back to login. */
  sessionExpired: boolean;
  /** Dismiss the "session expired" notice once it has been shown. */
  acknowledgeSessionExpired: () => void;
  /** Sign out: clears the session and its persisted copy. */
  signOut: () => void;
}

export const ConfigContext = createContext<ConfigContextValue | null>(null);

export function useAuth(): ConfigContextValue {
  const ctx = useContext(ConfigContext);
  if (!ctx) throw new Error('useAuth must be used within a ConfigProvider');
  return ctx;
}
