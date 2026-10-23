import { useState } from 'react';
import ErrorBanner from './ErrorBanner';
import { APIError } from '../types/api';
import { testConnection } from '../services/api';
import { useAuth } from '../services/authContext';

// Full-screen sign-in (ISI-6580). Shown until a session exists: tenant id
// (required) + optional bearer token in a collapsed "advanced" section. The
// connection is tested on submit; on failure the server's error is shown
// inline, on success the session is persisted and the app loads.
export default function LoginPage() {
  const { auth, setAuth, sessionExpired, acknowledgeSessionExpired } = useAuth();
  const [tenantId, setTenantId] = useState(auth.tenantId);
  const [token, setToken] = useState(auth.token ?? '');
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [error, setError] = useState<APIError | null>(null);
  const [testing, setTesting] = useState(false);

  async function onSubmit(event: React.FormEvent) {
    event.preventDefault();
    setError(null);
    if (tenantId.trim() === '') {
      acknowledgeSessionExpired();
      setError({ error: 'Tenant id is required.', code: 'validation', retriable: false });
      return;
    }
    setTesting(true);
    try {
      await testConnection({ tenantId: tenantId.trim(), token: token.trim() });
      setAuth({ tenantId: tenantId.trim(), token: token.trim() });
    } catch (err) {
      setError(err as APIError);
    } finally {
      setTesting(false);
    }
  }

  return (
    <div className="login-page">
      <form className="login-card" onSubmit={onSubmit}>
        <h1>🔍 MemKeys Inspector</h1>
        <p className="login-sub">Sign in with your tenant to inspect its memory.</p>

        {sessionExpired && (
          <div className="session-expired-notice" role="status">
            Session expired — please sign in again.
          </div>
        )}

        <label className="login-field">
          Tenant
          <input
            type="text"
            value={tenantId}
            placeholder="tenant id (X-MemKeys-Tenant)"
            autoFocus
            onChange={(e) => setTenantId(e.target.value)}
            disabled={testing}
          />
        </label>

        <button
          type="button"
          className="link-button login-advanced-toggle"
          onClick={() => setShowAdvanced((s) => !s)}
        >
          {showAdvanced ? 'Hide advanced' : 'Advanced (bearer token)'}
        </button>

        {showAdvanced && (
          <label className="login-field">
            Bearer token
            <input
              type="password"
              value={token}
              placeholder="optional JWT — never shown in plaintext"
              onChange={(e) => setToken(e.target.value)}
              disabled={testing}
            />
          </label>
        )}

        {error && <ErrorBanner error={error} />}

        <button type="submit" className="login-submit" disabled={testing}>
          {testing ? 'Testing connection…' : 'Sign in'}
        </button>
      </form>
    </div>
  );
}
