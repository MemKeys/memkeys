import { useState } from 'react';
import { useAuth } from '../services/authContext';

// A compact bar for the tenant id and optional bearer token. The token field
// is masked and collapsed by default (dev loopback mode needs only a tenant).
export default function ConnectionBar() {
  const { auth, setAuth } = useAuth();
  const [showToken, setShowToken] = useState(false);

  return (
    <div className="connection-bar">
      <label>
        Tenant
        <input
          type="text"
          value={auth.tenantId}
          placeholder="tenant id (X-MemKeys-Tenant)"
          onChange={(e) => setAuth({ ...auth, tenantId: e.target.value })}
        />
      </label>
      <button
        type="button"
        className="link-button"
        onClick={() => setShowToken((s) => !s)}
      >
        {showToken ? 'Hide token' : 'Bearer token'}
      </button>
      {showToken && (
        <label>
          Token
          <input
            type="password"
            value={auth.token ?? ''}
            placeholder="optional JWT (Authorization: Bearer)"
            onChange={(e) => setAuth({ ...auth, token: e.target.value })}
          />
        </label>
      )}
    </div>
  );
}
