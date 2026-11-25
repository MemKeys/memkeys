import { useCallback, useEffect, useState } from 'react';
import { APIError, IngestReceipt, LinterDraft, LinterRecord, Severity } from '../types/api';
import { lintersAPI } from '../services/api';
import { useAuth } from '../services/authContext';
import {
  draftFromLinter,
  emptyDraft,
  filterLinters,
  linterCounts,
  LinterFilter,
  matchSummary,
  ScopeFilter,
} from '../lib/linters';
import ErrorBanner from './ErrorBanner';
import LinterEditor, { LinterToast, LinterToggleConfirm } from './LinterEditor';

const SCOPES: ScopeFilter[] = ['all', 'global', 'project'];

type EditorState =
  | { kind: 'closed' }
  | { kind: 'create' }
  | { kind: 'edit'; linter: LinterRecord };

type ConfirmState = { linter: LinterRecord; disabling: boolean } | null;

/**
 * Linters management view (Golden Path F, ISI-7816) — the Inspector's first
 * write surface. Lists linters (project scope via op-2, global scope via the
 * applicable_linters resolver), with create / edit / enable-disable through
 * op-1 `POST /memory/facts`. Advisory v1: enabled linters ride with facts on
 * fetch/search, they never block a write.
 */
export default function LintersBrowser() {
  const { auth } = useAuth();
  // A project context is required to key the listing (see lintersAPI.fetchLinters).
  const [project, setProject] = useState('memkeys');
  const [linters, setLinters] = useState<LinterRecord[] | null>(null);
  const [error, setError] = useState<APIError | null>(null);
  const [loading, setLoading] = useState(false);

  const [filter, setFilter] = useState<LinterFilter>({ scope: 'all', severity: '', text: '' });
  const [selected, setSelected] = useState<LinterRecord | null>(null);
  const [editor, setEditor] = useState<EditorState>({ kind: 'closed' });
  const [confirm, setConfirm] = useState<ConfirmState>(null);
  const [toast, setToast] = useState<IngestReceipt | null>(null);
  const [togglingId, setTogglingId] = useState<string | null>(null);

  const load = useCallback(
    async (p: string) => {
      setLoading(true);
      setError(null);
      try {
        const list = await lintersAPI.fetchLinters(p, auth);
        setLinters(list);
      } catch (err) {
        setError(err as APIError);
        setLinters(null);
      } finally {
        setLoading(false);
      }
    },
    [auth],
  );

  useEffect(() => {
    load(project);
    // Initial load only; subsequent loads are driven by explicit actions.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const counts = linters ? linterCounts(linters) : { total: 0, enabled: 0, global: 0, project: 0 };
  const scopeCount = (s: ScopeFilter) =>
    s === 'all' ? counts.total : s === 'global' ? counts.global : counts.project;
  const shown = linters ? filterLinters(linters, filter) : [];

  function onSaved(receipt: IngestReceipt) {
    setEditor({ kind: 'closed' });
    setToast(receipt);
    setSelected(null);
    load(project);
  }

  async function doToggle(linter: LinterRecord, disabling: boolean) {
    setTogglingId(linter.factId);
    setError(null);
    try {
      const draft: LinterDraft = { ...draftFromLinter(linter), enabled: !disabling };
      const receipt = await lintersAPI.storeLinter(
        draft,
        new Date().toISOString(),
        `ui-${Date.now()}`,
        auth,
      );
      setConfirm(null);
      setToast(receipt);
      await load(project);
    } catch (err) {
      setConfirm(null);
      setError(err as APIError);
    } finally {
      setTogglingId(null);
    }
  }

  return (
    <div className="linters-browser">
      <div className="linters-toolbar">
        <h2>Linters</h2>
        {linters && (
          <span className="count">
            {counts.total} linter{counts.total === 1 ? '' : 's'} · {counts.enabled} enabled
          </span>
        )}
        <span className="pill-group" role="group" aria-label="Scope filter">
          {SCOPES.map((s) => (
            <button
              key={s}
              type="button"
              className={`pill ${filter.scope === s ? 'on' : ''}`}
              onClick={() => setFilter((f) => ({ ...f, scope: s }))}
            >
              {s === 'all' ? 'All' : s === 'global' ? 'Global' : 'Project'}
              {linters && <span className="n">{scopeCount(s)}</span>}
            </button>
          ))}
        </span>
        <label>
          severity
          <select
            value={filter.severity}
            onChange={(e) => setFilter((f) => ({ ...f, severity: e.target.value as Severity | '' }))}
          >
            <option value="">(any)</option>
            <option value="info">info</option>
            <option value="warn">warn</option>
            <option value="error">error</option>
          </select>
        </label>
        <label className="grow">
          filter
          <input
            type="text"
            placeholder="name or rule text…"
            value={filter.text}
            onChange={(e) => setFilter((f) => ({ ...f, text: e.target.value }))}
          />
        </label>
        <label>
          project
          <input
            type="text"
            placeholder="e.g. memkeys"
            value={project}
            onChange={(e) => setProject(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') load(project);
            }}
          />
        </label>
        <button type="button" className="link-button" disabled={loading} onClick={() => load(project)}>
          {loading ? 'Loading…' : 'Reload'}
        </button>
        <span className="toolbar-spacer" />
        <button type="button" onClick={() => setEditor({ kind: 'create' })}>
          + New linter
        </button>
      </div>

      <p className="advisory-note">
        Advisory v1: enabled linters are returned alongside facts on fetch/search (
        <span className="mono">applicable_linters</span>) — they never block a write. Disable to
        retire a rule without deleting it.
      </p>

      {error && <ErrorBanner error={error} />}

      {linters && (
        <div className="results">
          <div className="results-count">
            {shown.length} linter(s) · type=linter · valid=current
          </div>
          {shown.length === 0 ? (
            <p className="empty linters-empty">
              <b>No linters{filter.scope === 'all' ? ' in this tenant yet' : ' for this scope'}.</b>
              <br />
              Define the first rule an agent must follow — e.g. “never build new code in Rust”.
              <br />
              <br />
              <button type="button" onClick={() => setEditor({ kind: 'create' })}>
                + New linter
              </button>
            </p>
          ) : (
            <table className="facts-table">
              <thead>
                <tr>
                  <th />
                  <th>linter</th>
                  <th>severity</th>
                  <th>scope</th>
                  <th>match</th>
                  <th>entity keys</th>
                  <th>event_time</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {shown.map((l) => (
                  <tr
                    key={l.factId}
                    className={`${selected?.factId === l.factId ? 'selected' : ''} ${
                      l.enabled ? '' : 'linter-off'
                    }`}
                    onClick={() => setSelected(l)}
                  >
                    <td onClick={(e) => e.stopPropagation()}>
                      <button
                        type="button"
                        className={`toggle ${l.enabled ? '' : 'off'}`}
                        disabled={togglingId === l.factId}
                        aria-label={`${l.enabled ? 'Disable' : 'Enable'} ${l.name}`}
                        onClick={() => setConfirm({ linter: l, disabling: l.enabled })}
                      />
                    </td>
                    <td>
                      <div className="linter-name">
                        {l.name || '—'}
                        {!l.enabled && <span className="badge badge-muted">disabled</span>}
                      </div>
                      <div className="rule-preview">{l.rule}</div>
                    </td>
                    <td>
                      {l.severity ? (
                        <span className={`sev sev-${l.severity}`}>{l.severity}</span>
                      ) : (
                        <span className="sev state-off">unknown</span>
                      )}
                    </td>
                    <td>
                      {l.scopeKind === 'global' ? (
                        <span className="scope-badge scope-global">global</span>
                      ) : (
                        <span className="scope-badge scope-project">project:{l.project}</span>
                      )}
                    </td>
                    <td className="keys-cell">{matchSummary(l.match)}</td>
                    <td className="keys-cell">
                      {l.entityKeys.length === 0
                        ? '—'
                        : l.entityKeys.map(([k, v]) => `${k}=${v}`).join(' ')}
                    </td>
                    <td>{l.eventTime ? new Date(l.eventTime).toLocaleString() : '—'}</td>
                    <td className="row-actions" onClick={(e) => e.stopPropagation()}>
                      <button
                        type="button"
                        className="link-button"
                        onClick={() => setEditor({ kind: 'edit', linter: l })}
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        className="link-button"
                        onClick={() => setConfirm({ linter: l, disabling: l.enabled })}
                      >
                        {l.enabled ? 'Disable' : 'Enable'}
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}

      {selected && (
        <LinterDetail
          linter={selected}
          onEdit={() => setEditor({ kind: 'edit', linter: selected })}
          onToggle={() => setConfirm({ linter: selected, disabling: selected.enabled })}
          onClose={() => setSelected(null)}
        />
      )}

      {editor.kind !== 'closed' && (
        <LinterEditor
          mode={editor.kind}
          initial={editor.kind === 'edit' ? draftFromLinter(editor.linter) : emptyDraft(project)}
          onSaved={onSaved}
          onClose={() => setEditor({ kind: 'closed' })}
        />
      )}

      {confirm && (
        <LinterToggleConfirm
          name={confirm.linter.name}
          factId={confirm.linter.factId}
          disabling={confirm.disabling}
          busy={togglingId === confirm.linter.factId}
          onConfirm={() => doToggle(confirm.linter, confirm.disabling)}
          onClose={() => setConfirm(null)}
        />
      )}

      {toast && <LinterToast receipt={toast} onClose={() => setToast(null)} />}
    </div>
  );
}

/** Linter detail popup (mock 5B) — the same modal shell as the graph's
 *  DetailPopup, rendering the linter fact's fields + rule body JSON. */
function LinterDetail({
  linter,
  onEdit,
  onToggle,
  onClose,
}: {
  linter: LinterRecord;
  onEdit: () => void;
  onToggle: () => void;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  async function copyId() {
    try {
      await navigator.clipboard.writeText(linter.factId);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  }

  const bodyText = linter.bodyJson
    ? JSON.stringify(linter.bodyJson, null, 2)
    : linter.rawBody;

  return (
    <div className="detail-overlay" onClick={onClose} role="presentation">
      <div
        className="detail-popup"
        role="dialog"
        aria-label="Linter detail"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="detail-popup-header">
          <h3>Linter detail</h3>
          <button type="button" className="detail-close" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </div>
        <dl className="detail-kv">
          <dt>type</dt>
          <dd>
            <span className="badge">linter</span>
            {linter.scopeKind === 'project' && <span className="badge badge-muted">keyed</span>}
          </dd>
          <dt>name</dt>
          <dd className="mono">{linter.name}</dd>
          <dt>severity</dt>
          <dd>
            {linter.severity ? (
              <span className={`sev sev-${linter.severity}`}>{linter.severity}</span>
            ) : (
              <span className="sev state-off">unknown</span>
            )}
          </dd>
          <dt>scope</dt>
          <dd>
            {linter.scopeKind === 'global' ? (
              <span className="scope-badge scope-global">global</span>
            ) : (
              <span className="scope-badge scope-project">project:{linter.project}</span>
            )}
          </dd>
          <dt>enabled</dt>
          <dd>
            {linter.enabled ? '✓ enabled' : '✕ disabled'}{' '}
            <span className="detail-action-note">(toggle writes a superseding record)</span>
          </dd>
          <dt>match</dt>
          <dd>{matchSummary(linter.match)}</dd>
          <dt>rule body</dt>
          <dd>
            <pre className="fact-body">{bodyText}</pre>
          </dd>
          {linter.eventTime && (
            <>
              <dt>event time</dt>
              <dd className="mono">{linter.eventTime}</dd>
            </>
          )}
          {linter.supersedes && (
            <>
              <dt>supersedes</dt>
              <dd className="mono">{linter.supersedes}</dd>
            </>
          )}
          {linter.provenance && (
            <>
              <dt>provenance</dt>
              <dd className="mono">{linter.provenance}</dd>
            </>
          )}
          <dt>fact id</dt>
          <dd className="mono">{linter.factId}</dd>
        </dl>
        <div className="detail-actions">
          <button type="button" onClick={onEdit}>
            Edit linter
          </button>
          <button type="button" className="link-button" onClick={onToggle}>
            {linter.enabled ? 'Disable' : 'Enable'}
          </button>
          <button type="button" className="link-button" onClick={copyId}>
            Copy fact id
          </button>
          {copied && <span className="detail-action-note">copied</span>}
        </div>
      </div>
    </div>
  );
}
