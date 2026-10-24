import { useAuth } from '../services/authContext';

// Header identity chip (ISI-6580): replaces the old ConnectionBar header
// inputs — shows the signed-in tenant and a sign-out action.
export default function IdentityChip() {
  const { auth, signOut } = useAuth();
  return (
    <div className="identity-chip">
      <span className="identity-tenant" title="Signed-in tenant">
        👤 {auth.tenantId}
      </span>
      <button type="button" className="link-button" onClick={signOut}>
        Sign out
      </button>
    </div>
  );
}
