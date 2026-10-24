import axios, { AxiosInstance } from 'axios';
import {
  APIError,
  FactsQuery,
  FactsResponse,
  TraverseRequest,
  TraverseResult,
  SearchQuery,
  SearchResponse,
} from '../types/api';
import { buildQueryParams, buildSearchBody } from '../lib/transform';

// All reads go through the `/memory` prefix. In dev, vite proxies this to the
// server (see vite.config.ts); in a deployed build the SPA is served from the
// same origin as the API, so a relative base works there too.
const API_BASE_URL = '/memory';

/** Server auth mode (http.rs): a verified bearer JWT, or open loopback-dev
 *  where the tenant is self-asserted via the `X-MemKeys-Tenant` header. */
export interface AuthConfig {
  tenantId: string;
  /** Optional Paperclip run-token JWT; sent as `Authorization: Bearer ***` */
  token?: string;
}

function authHeaders(auth: AuthConfig): Record<string, string> {
  const h: Record<string, string> = {};
  if (auth.tenantId.trim() !== '') h['X-MemKeys-Tenant'] = auth.tenantId.trim();
  if (auth.token && auth.token.trim() !== '') h['Authorization'] = `Bearer ${auth.token.trim()}`;
  return h;
}

// --- 401 session-expiry signal (ISI-6580) ---------------------------------
// Any API 401 means the session (tenant/token) is no longer accepted by the
// server. The response interceptor below fans the event out to registered
// listeners; the config provider subscribes and clears the persisted session,
// which returns the app to the login page with a "session expired" notice.
type UnauthorizedListener = () => void;
const unauthorizedListeners = new Set<UnauthorizedListener>();

function notifyUnauthorized(): void {
  unauthorizedListeners.forEach((listener) => {
    try {
      listener();
    } catch {
      // a broken listener must not break the fan-out
    }
  });
}

/** Subscribe to API 401 events; returns an unsubscribe function. */
export function onUnauthorized(listener: UnauthorizedListener): () => void {
  unauthorizedListeners.add(listener);
  return () => {
    unauthorizedListeners.delete(listener);
  };
}

// One shared client so the 401 interceptor covers every request.
const client: AxiosInstance = axios.create({
  baseURL: API_BASE_URL,
  timeout: 30000,
  headers: { 'Content-Type': 'application/json' },
});

client.interceptors.response.use(
  (response) => response,
  (error: unknown) => {
    const status = (error as { response?: { status?: number } }).response?.status;
    if (status === 401) notifyUnauthorized();
    return Promise.reject(error);
  },
);

/** Normalize an axios failure into the WS-5 §7 {@link APIError} shape. */
function toAPIError(error: unknown, fallback: string): APIError {
  const e = error as { response?: { status: number; data?: Partial<APIError> } };
  if (e.response) {
    const data = e.response.data ?? {};
    return {
      error: data.error || fallback,
      code: data.code || String(e.response.status),
      retriable: data.retriable ?? e.response.status === 429,
    };
  }
  return { error: fallback, retriable: true };
}

export const factsAPI = {
  /** Keyed retrieval (op 1). At least one entity key must be set upstream. */
  async fetchFacts(query: FactsQuery, auth: AuthConfig): Promise<FactsResponse> {
    try {
      const { data } = await client.get<FactsResponse>('/facts', {
        params: buildQueryParams(query),
        headers: authHeaders(auth),
      });
      return data;
    } catch (error) {
      throw toAPIError(error, 'Failed to fetch facts');
    }
  },
};

export const graphAPI = {
  /** Graph traversal (op 3). The body never carries a tenant (§7.3). */
  async traverse(request: TraverseRequest, auth: AuthConfig): Promise<TraverseResult> {
    try {
      const { data } = await client.post<TraverseResult>('/graph/traverse', request, {
        headers: authHeaders(auth),
      });
      return data;
    } catch (error) {
      throw toAPIError(error, 'Failed to traverse graph');
    }
  },
};

/** Connection test used by the login page (ISI-6580): the cheapest
 * authenticated read. Resolves when the server accepts the tenant/token,
 * throws an {@link APIError} carrying the server's reason otherwise. */
export async function testConnection(auth: AuthConfig): Promise<void> {
  try {
    await client.get('/facts', { params: { limit: 1 }, headers: authHeaders(auth) });
  } catch (error) {
    throw toAPIError(error, 'Connection test failed');
  }
}

export const searchAPI = {
  /**
   * Keyless semantic search (op 4). No entity keys required — the scope is
   * server-floored to the caller's tenant. Answers 501 feature_disabled
   * when memory.semantic is off; the Explore tab degrades to its
   * enable-hint empty state + Keyed-lookup fallback (ISI-6581) rather
   * than showing a raw error.
   */
  async search(query: SearchQuery, auth: AuthConfig): Promise<SearchResponse> {
    try {
      const { data } = await client.post<SearchResponse>(
        '/search',
        buildSearchBody(query),
        { headers: authHeaders(auth) },
      );
      return data;
    } catch (error) {
      throw toAPIError(error, 'Semantic search failed');
    }
  },
};
