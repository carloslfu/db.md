// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";
import * as api from "../apiClient";
import type { SchemaField, StoreStats, ValidateReport } from "../types";

// The store about itself: is it valid, what shape is it, what contract
// does it declare. Everything here is served by the store's own verbs —
// nothing is app state — and it re-checks live as files change (hand-edit
// a record to a bogus status and watch the health row appear).

function humanBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function fieldConstraints(field: SchemaField): string[] {
  const chips: string[] = [];
  if (field.required) chips.push("required");
  if (field.enum) chips.push(`enum: ${field.enum.join(" · ")}`);
  if (field.shape) chips.push(field.shape);
  return chips;
}

export function StoreView({ schema }: { schema: SchemaField[] }) {
  const [version, setVersion] = useState<string | null>(null);
  const [report, setReport] = useState<ValidateReport | null>(null);
  const [stats, setStats] = useState<StoreStats | null>(null);

  useEffect(() => {
    const check = () => {
      api.fetchValidate().then(setReport, () => {});
      api.fetchStats().then(setStats, () => {});
    };
    api.fetchVersion().then(setVersion, () => {});
    check();
    // Re-check on any store change, softly debounced so a burst of writes
    // (toggle-all, clear-completed) validates once.
    let timer: number | undefined;
    const offChange = api.onStoreChange(() => {
      if (timer !== undefined) window.clearTimeout(timer);
      timer = window.setTimeout(check, 700);
    });
    return () => {
      if (timer !== undefined) window.clearTimeout(timer);
      offChange();
    };
  }, []);

  const healthy = report !== null && report.summary.total === 0;

  return (
    <div className="view-panel store-view">
      <section className="store-section">
        <h3>server</h3>
        <div className="store-row">
          <code>dbmd api</code>
          {version && <span className="store-chip">v{version}</span>}
          <span className="store-muted">{api.API_BASE}</span>
        </div>
        <p className="store-note">
          Not an app server — the db.md toolkit serving this store's verbs over HTTP. Delete this
          page and the database stays fully operable.
        </p>
      </section>

      <section className="store-section">
        <h3>health</h3>
        {report === null ? null : healthy ? (
          <div className="store-row">
            <span className="health-ok">✓ valid</span>
            <span className="store-muted">
              schema, links, frontmatter — {stats ? `${stats.total_files} files checked` : "checked"}
            </span>
          </div>
        ) : (
          <ul className="health-issues">
            {report.issues.map((issue, index) => (
              <li key={index} className={`severity-${issue.severity}`}>
                <span className="issue-code">{issue.code}</span>
                <span className="issue-file">{issue.file}</span>
                <span className="issue-message">{issue.message}</span>
                {issue.suggestion && <span className="issue-suggestion">{issue.suggestion}</span>}
              </li>
            ))}
          </ul>
        )}
        <p className="store-note">
          Live <code>dbmd validate --all</code> — edit a record file by hand to a bogus{" "}
          <code>status</code> and this turns red within a second.
        </p>
      </section>

      {stats && (
        <section className="store-section">
          <h3>records</h3>
          <div className="store-row store-stats">
            <span>
              <strong>{stats.total_files}</strong> files
            </span>
            {Object.entries(stats.type_distribution).map(([type, count]) => (
              <span key={type}>
                <strong>{count}</strong> {type}
              </span>
            ))}
            <span>
              <strong>{humanBytes(stats.total_size_bytes)}</strong> on disk
            </span>
            {stats.broken_link_count > 0 && (
              <span className="stat-bad">
                <strong>{stats.broken_link_count}</strong> broken links
              </span>
            )}
          </div>
          <p className="store-note">Plain markdown files — the whole database is readable as-is.</p>
        </section>
      )}

      {schema.length > 0 && (
        <section className="store-section">
          <h3>schema</h3>
          <ul className="schema-fields">
            <li className="schema-type">### todo</li>
            {schema.map((field) => (
              <li key={field.name}>
                <span className="schema-name">{field.name}</span>
                {fieldConstraints(field).map((chip) => (
                  <span key={chip} className="store-chip">
                    {chip}
                  </span>
                ))}
              </li>
            ))}
          </ul>
          <p className="store-note">
            Declared in <code>db/DB.md</code>, enforced by the store, read live by this app — the
            priority picker on each todo row exists because the schema above says so. Add an enum
            field there and a control appears here with no app change.
          </p>
        </section>
      )}

      <div className="view-hint">
        the same answers in a shell: <code>dbmd validate --all</code> · <code>dbmd stats</code> ·{" "}
        <code>dbmd schema</code>
      </div>
    </div>
  );
}
