import { useState } from 'react';
import { FactRecord } from '../types/api';
import { recordEntityKeys } from '../lib/transform';

/**
 * Shared fact-card surface: the results table plus the expandable detail
 * card. Extracted verbatim from the Facts Browser (ISI-6581) so the keyless
 * Explore tab renders semantic hits through the same component; scores (set
 * for semantic hits) adds a score column. Row selection is local state: a
 * new result set (new array identity) clears it, matching the old browser
 * behavior of resetting on every search.
 */
export default function FactResults({
  facts,
  scores,
  emptyLabel,
}: {
  facts: FactRecord[];
  /** Fused similarity score per fact id; present only for Explore results. */
  scores?: Map<string, number>;
  emptyLabel: string;
}) {
  const [selected, setSelected] = useState<FactRecord | null>(null);
  const [rendered, setRendered] = useState(facts);

  if (rendered !== facts) {
    setRendered(facts);
    setSelected(null);
  }

  return (
    <div className="results">
      <div className="results-count">{facts.length} fact(s)</div>
      <table className="facts-table">
        <thead>
          <tr>
            {scores && <th>score</th>}
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
              {scores && <td className="score-cell">{scores.get(f.id)?.toFixed(3)}</td>}
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
      {facts.length === 0 && <p className="empty">{emptyLabel}</p>}

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
