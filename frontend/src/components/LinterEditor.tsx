import { useEffect, useMemo, useState } from 'react';
import { APIError, IngestReceipt, LinterDraft, Severity } from '../types/api';
import { lintersAPI } from '../services/api';
import { useAuth } from '../services/authContext';
import {
  RULE_MAX_LEN,
  SEVERITIES,
  validateDraft,
  wirePreview,
} from '../lib/linters';
import ErrorBanner from './ErrorBanner';

/** A fresh idempotency suffix per save attempt (retries of one click collapse). */
function idemSuffix(): string {
  return `ui-${Date.now()}`;
}

/**
 * Create / edit a linter (mocks 6A / 6B). Reuses the `.detail-overlay` /
 * `.detail-popup` shell (Esc / overlay / ✕ dismiss). All real logic — envelope
 * building, validation, slug grammar — lives in `lib/linters.ts` (unit-tested);
 * this component is the form + the op-1 write. `mode==='edit'` locks the name
 * (the supersession grain) and shows the supersedes note.
 */
export default function LinterEditor({
  mode,
  initial,
  onSaved,
  onClose,
}: {
  mode: 'create' | 'edit';
  initial: LinterDraft;
  onSaved: (receipt: IngestReceipt, draft: LinterDraft) => void;
  onClose: () => void;
}) {
  const { auth } = useAuth();
  const [draft, setDraft] = useState<LinterDraft>(initial);
  const [error, setError] = useState<APIError | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const errors = validateDraft(draft);
  const valid = Object.keys(errors).length === 0;
  const preview = useMemo(
    () => wirePreview(draft, new Date().toISOString(), 'ui-N'),
    [draft],
  );

  const set = <K extends keyof LinterDraft>(k: K, v: LinterDraft[K]) =>
    setDraft((d) => ({ ...d, [k]: v }));

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!valid || saving) return;
    setSaving(true);
    setError(null);
    try {
      const receipt = await lintersAPI.storeLinter(
        draft,
        new Date().toISOString(),
        idemSuffix(),
        auth,
      );
      onSaved(receipt, draft);
    } catch (err) {
      setError(err as APIError);
    } finally {
      setSaving(false);
    }
  }

  const ruleLen = draft.rule.trim().length;

  return (
    <div className="detail-overlay" onClick={onClose} role="presentation">
      <div
        className="detail-popup"
        role="dialog"
        aria-label={mode === 'create' ? 'New linter' : 'Edit linter'}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="detail-popup-header">
          <h3>
            {mode === 'create' ? 'New linter' : 'Edit linter — '}
            {mode === 'edit' && <span className="mono scope-project-text">linter:{draft.name}</span>}
          </h3>
          <button type="button" className="detail-close" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </div>

        {mode === 'edit' && (
          <div className="supersede-note">
            Saving writes a <b>new version</b> with <span className="mono">supersedes</span> ={' '}
            <span className="mono">{draft.supersedes?.slice(0, 8)}…</span>. The old version stays in
            history; readers only ever see the current one. A 409 conflict (someone saved first)
            prompts a reload instead of clobbering.
          </div>
        )}

        {error && <ErrorBanner error={error} />}

        <form onSubmit={submit} className="editor-form">
          <label className="editor-field">
            name <span className="req">*</span>
            <input
              type="text"
              value={draft.name}
              disabled={mode === 'edit'}
              placeholder="kebab-case slug"
              className={errors.name ? 'invalid' : ''}
              onChange={(e) => set('name', e.target.value)}
            />
            {errors.name ? (
              <span className="field-error">{errors.name}</span>
            ) : (
              <span className="field-hint">
                stored as subject <span className="mono">linter:{draft.name || '<slug>'}</span> —
                lowercase letters, digits, dashes
                {mode === 'edit' && ' · name is the supersession identity and immutable'}
              </span>
            )}
          </label>

          <div className="editor-field">
            scope
            <div className="seg" role="group" aria-label="Scope">
              <button
                type="button"
                className={draft.scopeKind === 'global' ? 'on' : ''}
                onClick={() => set('scopeKind', 'global')}
              >
                Global — tenant floor
              </button>
              <button
                type="button"
                className={draft.scopeKind === 'project' ? 'on' : ''}
                onClick={() => set('scopeKind', 'project')}
              >
                Project…
              </button>
            </div>
            {draft.scopeKind === 'project' && (
              <input
                type="text"
                value={draft.project}
                placeholder="e.g. memkeys"
                className={errors.project ? 'invalid' : ''}
                onChange={(e) => set('project', e.target.value)}
              />
            )}
            {errors.project ? (
              <span className="field-error">{errors.project}</span>
            ) : (
              <span className="field-hint">
                global = no entity key (all agents in the tenant) · project = stored under{' '}
                <span className="mono">entity.project</span>
              </span>
            )}
          </div>

          <div className="editor-field">
            severity <span className="req">*</span>
            <div className="seg" role="group" aria-label="Severity">
              {SEVERITIES.map((s) => (
                <button
                  key={s}
                  type="button"
                  className={draft.severity === s ? 'on' : ''}
                  onClick={() => set('severity', s as Severity)}
                >
                  <span className={`sev-dot sev-dot-${s}`} />
                  {s}
                </button>
              ))}
            </div>
            <span className="field-hint">
              advisory only — severity is reported to the agent, it never blocks (v1)
            </span>
          </div>

          <label className="editor-field">
            rule text <span className="req">*</span>
            <textarea
              value={draft.rule}
              placeholder="e.g. Never introduce new Rust modules; extend existing crates instead."
              className={errors.rule ? 'invalid' : ''}
              onChange={(e) => set('rule', e.target.value)}
            />
            {errors.rule ? (
              <span className="field-error">{errors.rule}</span>
            ) : (
              <span className="field-hint">
                plain language, the agent reads this verbatim ·{' '}
                <span className="mono">
                  {ruleLen}/{RULE_MAX_LEN}
                </span>
              </span>
            )}
          </label>

          <details className="editor-match">
            <summary>Applies to (optional narrowing)</summary>
            <div className="match-grid">
              <label className="editor-field">
                language
                <input
                  type="text"
                  value={draft.language}
                  placeholder="e.g. rust (blank = any)"
                  onChange={(e) => set('language', e.target.value)}
                />
              </label>
              <label className="editor-field">
                paths (comma-separated globs)
                <input
                  type="text"
                  value={draft.pathGlobs}
                  placeholder="e.g. crates/**, **/*.rs"
                  onChange={(e) => set('pathGlobs', e.target.value)}
                />
              </label>
            </div>
          </details>

          <label className="checkbox-row">
            <input
              type="checkbox"
              checked={draft.enabled}
              onChange={(e) => set('enabled', e.target.checked)}
            />
            Enabled — returned with retrieval results
          </label>

          <details className="wire">
            <summary>Wire preview — the exact request this form sends</summary>
            <pre>{preview}</pre>
          </details>

          <div className="editor-foot">
            <span className="write-note">
              op-1 <span className="mono">POST /memory/facts</span> with the session headers from
              login; 401 returns to the login page.
            </span>
            <button type="button" className="btn ghost" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn primary" disabled={!valid || saving}>
              {saving ? 'Saving…' : mode === 'create' ? 'Create linter' : 'Save new version'}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

/**
 * Small danger-confirm for enable/disable (mock 6C). The inline toggle and the
 * row Disable action route through here because each writes a superseding fact.
 */
export function LinterToggleConfirm({
  name,
  factId,
  disabling,
  onConfirm,
  onClose,
  busy,
}: {
  name: string;
  factId: string;
  /** true = turning off (danger copy); false = re-enabling. */
  disabling: boolean;
  onConfirm: () => void;
  onClose: () => void;
  busy: boolean;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div className="detail-overlay" onClick={onClose} role="presentation">
      <div
        className="detail-popup small"
        role="dialog"
        aria-label={disabling ? 'Disable linter' : 'Enable linter'}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="detail-popup-header">
          <h3>
            {disabling ? 'Disable' : 'Enable'}{' '}
            <span className="mono scope-project-text">linter:{name}</span>?
          </h3>
          <button type="button" className="detail-close" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </div>
        <p className="confirm-body">
          Writes a new version with <span className="mono">enabled: {String(!disabling)}</span> that
          supersedes <span className="mono">{factId.slice(0, 8)}…</span>.{' '}
          {disabling
            ? 'Agents stop receiving this rule; the history is kept. There is no hard delete in the four-op surface — disable is the soft delete.'
            : 'Agents receive this rule again on their next retrieval.'}
        </p>
        <div className="editor-foot" style={{ marginTop: 0 }}>
          <button type="button" className="btn ghost" onClick={onClose}>
            Keep {disabling ? 'enabled' : 'disabled'}
          </button>
          <button
            type="button"
            className={disabling ? 'btn danger' : 'btn primary'}
            disabled={busy}
            onClick={onConfirm}
          >
            {busy ? 'Saving…' : disabling ? 'Disable linter' : 'Enable linter'}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Success toast showing the write receipt (mock 6C). Auto-dismiss is the
 *  caller's concern; this just renders. */
export function LinterToast({ receipt, onClose }: { receipt: IngestReceipt; onClose: () => void }) {
  return (
    <div className="toast" role="status">
      <div className="toast-title">Linter version stored</div>
      <div className="toast-body">
        <span className="mono">fact_id {receipt.fact_id.slice(0, 8)}…</span> ·{' '}
        <span className="mono">duplicate: {String(receipt.duplicate)}</span>
      </div>
      <button type="button" className="link-button toast-close" onClick={onClose}>
        dismiss
      </button>
    </div>
  );
}
