import { useState } from 'react';
import { APIError, ENTITY_KEY_NAMES, EntityKeyName, FactRecord, FactsQuery, Validity } from '../types/api';
import { factsAPI } from '../services/api';
import { useAuth } from '../services/authContext';
import { DEFAULT_LIMIT, hasEntityKey } from '../lib/transform';
import ErrorBanner from './ErrorBanner';
import FactResults from './FactResults';

// Canonical §5.1 fact types for the type filter.
const FACT_TYPES = ['decision', 'outcome', 'observation', 'learning', 'context'];

// One concrete placeholder per entity key (values verified live on
// k8squad-test) so the required exact-match values are discoverable in the
// UI itself - the board test (ISI-6395 B3) showed users guessing blind.
const KEY_PLACEHOLDERS: Record<EntityKeyName, string> = {
  project: 'e.g. memkeys',
  issue: 'e.g. ISI-6541',
  pr: 'e.g. 51',
  agent: 'e.g. backup_Architect',
  run: 'e.g. run id',
  thread: 'e.g. thread id',
  service: 'e.g. service name',
};

const emptyQuery: FactsQuery = {
  keys: {},
  type: '',
  as_of: '',
  valid: 'current',
  limit: DEFAULT_LIMIT,
};

/**
 * Keyed lookup (advanced) — keyed retrieval over `GET /memory/facts`. The
 * server requires at least one entity key, so the search button stays
 * disabled until one is provided (surfaced inline rather than letting the
 * server 400). The keyless Explore tab (ISI-6581, POST /memory/search) is
 * the default landing view; this exact-key browser remains for scoping.
 */
export default function FactsBrowser() {
  const { auth } = useAuth();
  const [query, setQuery] = useState<FactsQuery>(emptyQuery);
  const [facts, setFacts] = useState<FactRecord[] | null>(null);
  const [error, setError] = useState<APIError | null>(null);
  const [loading, setLoading] = useState(false);

  const setKey = (name: EntityKeyName, value: string) =>
    setQuery((q) => ({ ...q, keys: { ...q.keys, [name]: value } }));

  /** Shared query runner used by both the form submit and the example buttons. */
  async function doSearch(q: FactsQuery) {
    if (!hasEntityKey(q)) {
      setError({ error: 'Provide at least one entity key (keys are ANDed).' });
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await factsAPI.fetchFacts(q, auth);
      setFacts(res.facts);
    } catch (err) {
      setError(err as APIError);
      setFacts(null);
    } finally {
      setLoading(false);
    }
  }

  async function search(e: React.FormEvent) {
    e.preventDefault();
    await doSearch(query);
  }

  /** Fill the form with a known-good example query and run it immediately, so
   * a fresh visitor sees data with one click instead of guessing key values. */
  async function runExample(keys: Partial<Record<EntityKeyName, string>>) {
    const q: FactsQuery = { ...emptyQuery, keys };
    setQuery(q);
    await doSearch(q);
  }

  return (
    <div className="facts-browser">
      <form className="search-form" onSubmit={search}>
        <div className="key-grid">
          {ENTITY_KEY_NAMES.map((name) => (
            <label key={name}>
              {name}
              <input
                type="text"
                placeholder={KEY_PLACEHOLDERS[name]}
                value={query.keys[name] ?? ''}
                onChange={(e) => setKey(name, e.target.value)}
              />
            </label>
          ))}
        </div>
        <div className="filter-row">
          <label>
            type
            <select
              value={query.type ?? ''}
              onChange={(e) => setQuery((q) => ({ ...q, type: e.target.value }))}
            >
              <option value="">(any)</option>
              {FACT_TYPES.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          </label>
          <label>
            valid
            <select
              value={query.valid}
              onChange={(e) => setQuery((q) => ({ ...q, valid: e.target.value as Validity }))}
            >
              <option value="current">current</option>
              <option value="all">all (history)</option>
            </select>
          </label>
          <label>
            as_of
            <input
              type="text"
              placeholder="RFC3339 (optional)"
              value={query.as_of ?? ''}
              onChange={(e) => setQuery((q) => ({ ...q, as_of: e.target.value }))}
            />
          </label>
          <label>
            limit
            <input
              type="number"
              min={1}
              value={query.limit}
              onChange={(e) =>
                setQuery((q) => ({ ...q, limit: Number(e.target.value) || DEFAULT_LIMIT }))
              }
            />
          </label>
          <button type="submit" disabled={loading}>
            {loading ? 'Searching…' : 'Search'}
          </button>
        </div>
      </form>

      <div className="examples-row">
        <span>examples:</span>
        <button
          type="button"
          className="link-button"
          disabled={loading}
          onClick={() => runExample({ project: 'memkeys' })}
        >
          project=memkeys
        </button>
        <span className="hint">exact entity values required · tenant &amp; token: header bar</span>
      </div>

      {error && <ErrorBanner error={error} />}

      {facts && <FactResults facts={facts} emptyLabel="No facts for this query." />}
    </div>
  );
}
