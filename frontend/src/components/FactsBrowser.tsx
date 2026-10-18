import { useState } from 'react';
import { APIError, ENTITY_KEY_NAMES, EntityKeyName, FactRecord, FactsQuery, Validity } from '../types/api';
import { factsAPI } from '../services/api';
import { useAuth } from '../services/authContext';
import { DEFAULT_LIMIT, hasEntityKey, recordEntityKeys } from '../lib/transform';
import ErrorBanner from './ErrorBanner';

// Canonical §5.1 fact types for the type filter.
const FACT_TYPES = ['decision', 'outcome', 'observation', 'learning', 'context'];

const emptyQuery: FactsQuery = {
  keys: {},
  type: '',
  as_of: '',
  valid: 'current',
  limit: DEFAULT_LIMIT,
};

/**
 * Facts Browser — keyed retrieval over `GET /memory/facts`. The server requires
 * at least one entity key, so the search button stays disabled until one is
 * provided (surfaced inline rather than letting the server 400).
 */
export default function FactsBrowser() {
  const { auth } = useAuth();
  const [query, setQuery] = useState<FactsQuery>(emptyQuery);
  const [facts, setFacts] = useState<FactRecord[] | null>(null);
  const [error, setError] = useState<APIError | null>(null);
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<FactRecord | null>(null);

  const setKey = (name: EntityKeyName, value: string) =>
    setQuery((q) => ({ ...q, keys: { ...q.keys, [name]: value } }));

  async function search(e: React.FormEvent) {
    e.preventDefault();
    if (!hasEntityKey(query)) {
      setError({ error: 'Provide at least one entity key (keys are ANDed).' });
      return;
    }
    setLoading(true);
    setError(null);
    setSelected(null);
    try {
      const res = await factsAPI.fetchFacts(query, auth);
      setFacts(res.facts);
    } catch (err) {
      setError(err as APIError);
      setFacts(null);
    } finally {
      setLoading(false);
    }
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

      {error && <ErrorBanner error={error} />}

      {facts && (
        <div className="results">
          <div className="results-count">{facts.length} fact(s)</div>
          <table className="facts-table">
            <thead>
              <tr>
                <th>type</th>
                <th>subject</th>
                <th>entity keys</th>
                <th>event_time</th>
                <th>keyed</th>
              </tr>
            </thead>
            <tbody>
              {facts.map((f) => (
                <tr
                  key={f.id}
                  className={selected?.id === f.id ? 'selected' : ''}
                  onClick={() => setSelected(f)}
                >
                  <td>{f.fact_type}</td>
                  <td>{f.subject ?? '—'}</td>
                  <td className="keys-cell">
                    {recordEntityKeys(f)
                      .map(([k, v]) => `${k}=${v}`)
                      .join(' ')}
                  </td>
                  <td>{new Date(f.event_time).toLocaleString()}</td>
                  <td>{f.keyed ? '✓' : ''}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {facts.length === 0 && <p className="empty">No facts for this query.</p>}
        </div>
      )}

      {selected && (
        <div className="fact-detail">
          <div className="fact-detail-header">
            <h3>Fact detail</h3>
            <button type="button" className="link-button" onClick={() => setSelected(null)}>
              close
            </button>
          </div>
          <dl>
            <dt>id</dt>
            <dd className="mono">{selected.id}</dd>
            <dt>fact_type</dt>
            <dd>{selected.fact_type}</dd>
            <dt>confidence</dt>
            <dd>{selected.confidence ?? '—'}</dd>
            <dt>supersedes</dt>
            <dd className="mono">{selected.supersedes ?? '—'}</dd>
            <dt>provenance</dt>
            <dd>{selected.provenance_principal}</dd>
            <dt>ingested_at</dt>
            <dd>{new Date(selected.ingested_at).toLocaleString()}</dd>
          </dl>
          <h4>body</h4>
          <pre className="fact-body">{selected.body}</pre>
        </div>
      )}
    </div>
  );
}
