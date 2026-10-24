import { useState } from 'react';
import { APIError, FactRecord, SearchHit, SearchQuery } from '../types/api';
import { searchAPI } from '../services/api';
import { useAuth } from '../services/authContext';
import {
  DEFAULT_SEARCH_K,
  hasSearchText,
  hitToRecord,
  isFeatureDisabled,
} from '../lib/transform';
import ErrorBanner from './ErrorBanner';
import FactResults from './FactResults';

// Canonical fact types for the type filter (same list as Keyed lookup).
const FACT_TYPES = ['decision', 'outcome', 'observation', 'learning', 'context'];

const RESULT_COUNTS = [10, 25, 50];

const emptyQuery: SearchQuery = { query: '', type: '', k: DEFAULT_SEARCH_K };

/**
 * Explore — keyless fact discovery over `POST /memory/search` (ISI-6581). A
 * natural-language question is the only required input: the server scopes
 * every query to the caller's tenant, so no entity keys are needed (the
 * exact-match key pain point stays in the Keyed lookup tab). When the
 * semantic module is off the server answers 501 feature_disabled: render
 * the enable-memory.semantic empty state with a one-click fallback to Keyed
 * lookup — never a raw error.
 */
export default function ExploreBrowser({
  onUseKeyedLookup,
}: {
  /** Switches the app to the keyed Facts Browser (501 fallback path). */
  onUseKeyedLookup: () => void;
}) {
  const { auth } = useAuth();
  const [query, setQuery] = useState<SearchQuery>(emptyQuery);
  const [hits, setHits] = useState<SearchHit[] | null>(null);
  const [error, setError] = useState<APIError | null>(null);
  const [semanticOff, setSemanticOff] = useState(false);
  const [loading, setLoading] = useState(false);

  async function doSearch(q: SearchQuery) {
    if (!hasSearchText(q)) {
      setError({ error: 'Type a natural-language question first — no entity keys needed.' });
      return;
    }
    setLoading(true);
    setError(null);
    setSemanticOff(false);
    try {
      const res = await searchAPI.search(q, auth);
      setHits(res.hits);
    } catch (err) {
      const apiErr = err as APIError;
      if (isFeatureDisabled(apiErr)) {
        // Module off is a state to explain, not an error to flash.
        setSemanticOff(true);
        setHits(null);
      } else {
        setError(apiErr);
        setHits(null);
      }
    } finally {
      setLoading(false);
    }
  }

  async function search(e: React.FormEvent) {
    e.preventDefault();
    await doSearch(query);
  }

  const facts: FactRecord[] | null = hits ? hits.map(hitToRecord) : null;
  const scores = hits ? new Map(hits.map((h) => [h['fact.id'], h.score])) : undefined;

  return (
    <div className="explore-browser">
      <form className="search-form" onSubmit={search}>
        <div className="query-row">
          <label className="grow">
            ask anything
            <input
              type="text"
              className="query-input"
              placeholder="e.g. what did we decide about run tokens?"
              value={query.query}
              onChange={(e) => setQuery((q) => ({ ...q, query: e.target.value }))}
            />
          </label>
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
            results
            <select
              value={query.k}
              onChange={(e) => setQuery((q) => ({ ...q, k: Number(e.target.value) }))}
            >
              {RESULT_COUNTS.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
          <button type="submit" disabled={loading || !hasSearchText(query)}>
            {loading ? 'Searching…' : 'Search'}
          </button>
        </div>
      </form>

      <div className="examples-row">
        <span className="hint">no entity keys needed · plain-language question · tenant &amp; token: header bar</span>
      </div>

      {error && <ErrorBanner error={error} />}

      {semanticOff && (
        <div className="explore-disabled" role="note">
          <strong>Semantic search is disabled on this server.</strong>
          <p>
            Keyless Explore runs on <code>POST /memory/search</code>, which needs the{' '}
            <code>memory.semantic</code> module (embedder + vector index) enabled. Ask the
            operator to enable it; until then the exact-key browser still works.
          </p>
          <button type="button" onClick={onUseKeyedLookup}>
            Use Keyed lookup instead →
          </button>
        </div>
      )}

      {facts && (
        <FactResults facts={facts} scores={scores} emptyLabel="No facts matched this question." />
      )}
    </div>
  );
}
