import { useEffect, useState } from 'react';
import { EDGE_MEANINGS, EdgeDetail, StartDirection } from '../lib/legend';
import { GraphNode, recordEntityKeys } from '../lib/transform';

interface DetailPopupProps {
  node: GraphNode | null;
  edge: EdgeDetail | null;
  /** Node action (approved mock): restart the traversal from this node. */
  onTraverseFromHere?: (node: GraphNode) => void;
  onClose: () => void;
}

/**
 * Modal detail popup for the Memory Graph (ISI-6582): the hydrated fact behind
 * a clicked node, or the meaning, endpoints, direction and timestamp of a
 * clicked edge. Escape, overlay click, or the close button dismiss it.
 */
export default function DetailPopup({ node, edge, onTraverseFromHere, onClose }: DetailPopupProps) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  if (!node && !edge) return null;

  return (
    <div className="detail-overlay" onClick={onClose} role="presentation">
      <div
        className="detail-popup"
        role="dialog"
        aria-label="Graph detail"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="detail-popup-header">
          <h3>{node ? 'Fact detail' : 'Edge detail'}</h3>
          <button type="button" className="detail-close" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </div>

        {node && (
          <NodeDetail
            node={node}
            onTraverseFromHere={onTraverseFromHere}
            onClose={onClose}
          />
        )}
        {edge && <EdgeDetailBody edge={edge} />}
      </div>
    </div>
  );
}

function NodeActions({
  node,
  onTraverseFromHere,
  onClose,
}: {
  node: GraphNode;
  onTraverseFromHere?: (node: GraphNode) => void;
  onClose: () => void;
}) {
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');
  async function copyId() {
    try {
      await navigator.clipboard.writeText(node.id);
      setCopyState('copied');
    } catch {
      setCopyState('failed');
    }
  }
  return (
    <div className="detail-actions">
      {onTraverseFromHere && (
        <button
          type="button"
          onClick={() => {
            onClose();
            onTraverseFromHere(node);
          }}
        >
          Traverse from here
        </button>
      )}
      <button type="button" className="link-button" onClick={copyId}>
        Copy node id
      </button>
      {copyState === 'copied' && <span className="detail-action-note">copied</span>}
      {copyState === 'failed' && (
        <span className="detail-action-note">copy blocked — id is selectable above</span>
      )}
    </div>
  );
}

function NodeDetail({
  node,
  onTraverseFromHere,
  onClose,
}: {
  node: GraphNode;
  onTraverseFromHere?: (node: GraphNode) => void;
  onClose: () => void;
}) {
  const r = node.record;
  if (!r) {
    return (
      <div>
        <dl className="detail-kv">
          <dt>kind</dt>
          <dd className="mono">{node.kind}</dd>
          <dt>id</dt>
          <dd className="mono">{node.id}</dd>
        </dl>
        <p className="detail-note">
          {node.unhydratable
            ? 'Unhydratable node — the traverse could not load a record for this node; it is kept for topology only.'
            : 'Topology-only node — this traverse returned no hydrated record. Re-run with “hydrate” enabled to load full content.'}
        </p>
        <NodeActions node={node} onTraverseFromHere={onTraverseFromHere} onClose={onClose} />
      </div>
    );
  }
  const keys = recordEntityKeys(r);
  return (
    <>
      <dl className="detail-kv">
        <dt>type</dt>
      <dd>
        <span className="badge">{r.fact_type}</span>
        {r.keyed && <span className="badge badge-muted">keyed</span>}
      </dd>
      {r.subject && (
        <>
          <dt>subject</dt>
          <dd>{r.subject}</dd>
        </>
      )}
      <dt>body</dt>
      <dd>
        <div className="fact-body">{r.body}</div>
      </dd>
      <dt>event time</dt>
      <dd className="mono">{r.event_time}</dd>
      <dt>ingested at</dt>
      <dd className="mono">{r.ingested_at}</dd>
      {r.supersedes && (
        <>
          <dt>supersedes</dt>
          <dd className="mono">{r.supersedes}</dd>
        </>
      )}
      {r.confidence !== null && (
        <>
          <dt>confidence</dt>
          <dd>{r.confidence}</dd>
        </>
      )}
      <dt>provenance</dt>
      <dd className="mono">{r.provenance_principal}</dd>
      <dt>fact id</dt>
      <dd className="mono">{r.id}</dd>
      {keys.length > 0 && (
        <>
          <dt>entity keys</dt>
          <dd className="mono">
            {keys.map(([name, value]) => (
              <div key={name}>
                {name}: {value}
              </div>
            ))}
          </dd>
        </>
      )}
      </dl>
      <NodeActions node={node} onTraverseFromHere={onTraverseFromHere} onClose={onClose} />
    </>
  );
}

const DIRECTION_TEXT: Record<StartDirection, string> = {
  out: 'out — leaves the traversal start',
  in: 'in — enters the traversal start',
  both: 'self-loop on the traversal start',
};

function EdgeDetailBody({ edge }: { edge: EdgeDetail }) {
  const meaning = (EDGE_MEANINGS as Record<string, string | undefined>)[edge.edge_type];
  return (
    <dl className="detail-kv">
      <dt>meaning</dt>
      <dd>
        <span className="badge">{edge.edge_type}</span>{' '}
        {meaning ?? 'Unknown edge type.'}
      </dd>
      <dt>source</dt>
      <dd className="mono">{edge.source}</dd>
      <dt>target</dt>
      <dd className="mono">{edge.target}</dd>
      <dt>direction</dt>
      <dd>{edge.direction ? DIRECTION_TEXT[edge.direction] : 'not connected to the traversal start'}</dd>
      <dt>event time</dt>
      <dd className="mono">{edge.event_time}</dd>
    </dl>
  );
}
