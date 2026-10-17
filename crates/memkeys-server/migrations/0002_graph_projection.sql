-- MemKeys M2 — graph projection (ADR-GRAPH, docs/architecture.md §10; WS-2/ISI-5867 Accepted rev 4).
--
-- The graph is a derived, always-on, REBUILDABLE projection of the keyed store
-- (§5.4.1): relational edge-rows co-located with the SoT store, traversed by an
-- app-side bounded level-synchronous BFS (ADR-GRAPH Decision). Node ids are
-- tenant-namespaced opaque strings "{tenant}|{kind}|{local_id}" (percent-encoded
-- for '|' and '%') — no separate node table: nodes materialize through their
-- edges (the graph carries node ids + edge metadata only, never body text).
--
-- Edge vocabulary (ADR-GRAPH; the architecture §5.4.1 `temporal` edge was
-- DROPPED by the ADR — rev 2, HIGH-6): entity_fact | supersedes | produced_by |
-- a2a_participant.
--
-- Idempotency: PK (tenant, src_id, dst_id, edge_type) — upserts replay safely
-- (D3). `project` is functionally determined by the SOURCE FACT the edge was
-- derived from (not by the src node kind), so two edges can never differ only
-- in project (ADR-GRAPH rev 2, MED-10).

CREATE TABLE edges (
    tenant      TEXT        NOT NULL,   -- := principal.tenant_id (WS-5 §3); identity grain
    project     TEXT,                   -- functionally determined by the src fact; scope facet
    src_kind    TEXT        NOT NULL,   -- fact | entity.<dim> | a2a_hint
    src_id      TEXT        NOT NULL,   -- tenant-namespaced node id
    dst_kind    TEXT        NOT NULL,
    dst_id      TEXT        NOT NULL,
    edge_type   TEXT        NOT NULL
                            CHECK (edge_type IN
                            ('entity_fact', 'supersedes', 'produced_by', 'a2a_participant')),
    event_time  TIMESTAMPTZ NOT NULL,
    rule_ver    TEXT        NOT NULL,   -- RULE version (structural edges are rule-derived)
    PRIMARY KEY (tenant, src_id, dst_id, edge_type)
);

-- Per-level BFS set-fetch + scope filters ride these indexes (ADR-GRAPH §10):
-- event_time in the key so "agent A's decisions last sprint" range-scans ON the
-- graph instead of scanning all of A's edges (rev 2 — B4).
CREATE INDEX edges_out ON edges (tenant, project, src_id, edge_type, event_time);
CREATE INDEX edges_in  ON edges (tenant, project, dst_id, edge_type, event_time);

-- Projection-hole detection (Project's HIGH-9 precondition: a supersedes edge
-- is DEFERRED until both endpoints have an entity_fact projection) and the
-- worker's catch-up scan both ask "which committed facts have no entity_fact
-- edge yet" — the dst-side partial index keeps that an index-only lookup.
CREATE INDEX edges_entity_fact_dst ON edges (tenant, dst_id) WHERE edge_type = 'entity_fact';
