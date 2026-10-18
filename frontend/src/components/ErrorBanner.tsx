import { APIError } from '../types/api';

/** Render a WS-5 §7 error (code + retriable hint) as a dismissible banner. */
export default function ErrorBanner({ error }: { error: APIError }) {
  return (
    <div className="error-banner" role="alert">
      <strong>Error{error.code ? ` (${error.code})` : ''}:</strong> {error.error}
      {error.retriable ? <span className="retriable"> — retriable</span> : null}
    </div>
  );
}
