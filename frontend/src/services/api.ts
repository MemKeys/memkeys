import axios, { AxiosInstance } from 'axios';
import {
  APIError,
  FactsQuery,
  FactsResponse,
  TraverseRequest,
  TraverseResult,
} from '../types/api';
import { buildQueryParams } from '../lib/transform';

// All reads go through the `/memory` prefix. In dev, vite proxies this to the
// server (see vite.config.ts); in a deployed build the SPA is served from the
// same origin as the API, so a relative base works there too.
const API_BASE_URL = '/memory';

/** Server auth mode (http.rs): a verified bearer JWT, or open loopback-dev
 *  where the tenant is self-asserted via the `X-MemKeys-Tenant` header. */
export interface AuthConfig {
  tenantId: string;
  /** Optional Paperclip run-token JWT; sent as `Authorization: Bearer …`. */
  token?: string;
}

function authHeaders(auth: AuthConfig): Record<string, string> {
  const h: Record<string, string> = {};
  if (auth.tenantId.trim() !== '') h['X-MemKeys-Tenant'] = auth.tenantId.trim();
  if (auth.token && auth.token.trim() !== '') h['Authorization'] = `Bearer ${auth.token.trim()}`;
  return h;
}

function client(): AxiosInstance {
  return axios.create({
    baseURL: API_BASE_URL,
    timeout: 30000,
    headers: { 'Content-Type': 'application/json' },
  });
}

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
      const { data } = await client().get<FactsResponse>('/facts', {
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
      const { data } = await client().post<TraverseResult>('/graph/traverse', request, {
        headers: authHeaders(auth),
      });
      return data;
    } catch (error) {
      throw toAPIError(error, 'Failed to traverse graph');
    }
  },
};
