import { useRef, useState } from 'react';
import * as d3 from 'd3';
import {
  APIError,
  Direction,
  EdgeType,
  NodeKind,
  TraverseRequest,
  TraverseResult,
} from '../types/api';
import { graphAPI } from '../services/api';
import { useAuth } from '../services/authContext';
import { buildNodeId, GraphLink, GraphNode, toGraphData } from '../lib/transform';
import {
  EdgeDetail,
  EDGE_COLOR,
  EDGE_MEANINGS,
  edgeDirectionRelativeToStart,
  EDGE_TYPE_ORDER,
  KIND_COLOR,
  NODE_KIND_ORDER,
  supersededNodeIds,
} from '../lib/legend';
import ErrorBanner from './ErrorBanner';
import GraphLegend from './GraphLegend';
import DetailPopup from './DetailPopup';

const EDGE_TYPES: EdgeType[] = ['entity_fact', 'supersedes', 'produced_by', 'a2a_participant'];
const DIRECTIONS: Direction[] = ['out', 'in', 'both'];

type SimNode = GraphNode & d3.SimulationNodeDatum;
type SimLink = d3.SimulationLinkDatum<SimNode> & { edge_type: string; event_time: string };

/**
 * Endpoint id of a d3 link endpoint. Before the force layout initializes a
 * link, d3 leaves its endpoints as the raw datum (string id or link index);
 * afterwards they are resolved to the sim node.
 */
function linkEndId(v: string | number | SimNode): string {
  return typeof v === 'object' ? v.id : String(v);
}

/**
 * Memory Graph — renders the neighborhood returned by
 * `POST /memory/graph/traverse` as a d3 force-directed graph. Topology-only or
 * unhydratable nodes are still drawn (body omitted, never faked); dropped
 * dangling edges are surfaced in the status line. Clicking a node opens the
 * fact-detail popup, clicking an edge opens the edge-detail popup (ISI-6582);
 * a static legend under the canvas decodes edge semantics and node styles.
 */
export default function MemoryGraph() {
  const { auth } = useAuth();
  const svgRef = useRef<SVGSVGElement | null>(null);
  const [startKind, setStartKind] = useState<NodeKind>('entity_project');
  const [startId, setStartId] = useState('');
  const [startValue, setStartValue] = useState('');
  const [direction, setDirection] = useState<Direction>('both');
  const [depth, setDepth] = useState(2);
  const [hydrate, setHydrate] = useState(true);
  const [edges, setEdges] = useState<EdgeType[]>([]);
  const [error, setError] = useState<APIError | null>(null);
  const [loading, setLoading] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [selectedNode, setSelectedNode] = useState<GraphNode | null>(null);
  const [selectedEdge, setSelectedEdge] = useState<EdgeDetail | null>(null);
  /** The traverse start id, for edge-direction tooltips/popups (survives redraws). */
  const startIdRef = useRef<string | null>(null);

  const toggleEdge = (t: EdgeType) =>
    setEdges((cur) => (cur.includes(t) ? cur.filter((x) => x !== t) : [...cur, t]));

  /** Compose the opaque start id from the picker (tenant|kind|value). */
  function composeStartId(kind: NodeKind, value: string): string {
    return buildNodeId(auth.tenantId, kind, value);
  }

  /** Fill the picker with a known-good combo and traverse immediately, so a
   * fresh visitor sees data with one click instead of guessing id formats. */
  async function runExample() {
    const kind: NodeKind = 'entity_project';
    const value = 'memkeys';
    const id = composeStartId(kind, value);
    setStartKind(kind);
    setStartValue(value);
    setDirection('both');
    setDepth(2);
    setStartId(id);
    await traverse({ start: { kind, id }, direction: 'both', depth: 2 });
  }

  async function run(e: React.FormEvent) {
    e.preventDefault();
    if (startId.trim() === '') {
      setError({ error: 'A start node id is required.' });
      return;
    }
    await traverse({ start: { kind: startKind, id: startId.trim() } });
  }

  async function traverse(
    partial: Pick<TraverseRequest, 'start'> & Partial<Pick<TraverseRequest, 'direction' | 'depth'>>,
  ) {
    setLoading(true);
    setError(null);
    const req: TraverseRequest = {
      edges, // empty = all edge types
      direction,
      depth,
      hydrate,
      deadline_ms: 2000,
      ...partial,
    };
    startIdRef.current = req.start.id;
    setSelectedNode(null);
    setSelectedEdge(null);
    try {
      const result = await graphAPI.traverse(req, auth);
      draw(result);
    } catch (err) {
      setError(err as APIError);
      setStatus(null);
    } finally {
      setLoading(false);
    }
  }

  function draw(result: TraverseResult) {
    const { nodes, links, droppedEdges } = toGraphData(result);
    const parts = [`${nodes.length} nodes, ${links.length} edges`];
    if (result.truncated) parts.push(`truncated (${result.reason})`);
    if (droppedEdges > 0) parts.push(`${droppedEdges} dangling edge(s) hidden`);
    setStatus(parts.join(' · '));

    const el = svgRef.current;
    if (!el) return;
    const svg = d3.select(el);
    svg.selectAll('*').remove();
    const width = el.clientWidth || 800;
    const height = el.clientHeight || 520;

    const simNodes: SimNode[] = nodes.map((n) => ({ ...n }));
    const simLinks: SimLink[] = (links as GraphLink[]).map((l) => ({ ...l }));
    const superseded = supersededNodeIds(links);

    const sim = d3
      .forceSimulation<SimNode>(simNodes)
      .force(
        'link',
        d3
          .forceLink<SimNode, SimLink>(simLinks)
          .id((d) => d.id)
          .distance(90),
      )
      .force('charge', d3.forceManyBody().strength(-220))
      .force('center', d3.forceCenter(width / 2, height / 2))
      .force('collide', d3.forceCollide(26));

    // Per-type arrowheads (approved mock: edges show direction), one marker
    // per edge color; userSpaceOnUse keeps the head size off the 10px hit stroke.
    svg
      .append('defs')
      .selectAll('marker')
      .data(EDGE_TYPE_ORDER)
      .join('marker')
      .attr('id', (d) => `mk-arrow-${d}`)
      .attr('viewBox', '0 -5 10 10')
      .attr('refX', 19)
      .attr('refY', 0)
      .attr('markerWidth', 8)
      .attr('markerHeight', 8)
      .attr('orient', 'auto')
      .attr('markerUnits', 'userSpaceOnUse')
      .append('path')
      .attr('d', 'M0,-5L10,0L0,5')
      .attr('fill', (d) => EDGE_COLOR[d]);

    const root = svg.append('g');
    svg.call(
      d3
        .zoom<SVGSVGElement, unknown>()
        .scaleExtent([0.2, 4])
        .on('zoom', (ev) => root.attr('transform', ev.transform.toString())),
    );
    // Clicking the empty canvas dismisses any open detail popup.
    svg.on('click', () => {
      setSelectedNode(null);
      setSelectedEdge(null);
    });

    // Wide transparent hit-lines carry the hover tooltip and click for edges —
    // a 1.5px stroke is an impossible click target.
    const hit = root
      .append('g')
      .attr('stroke', 'transparent')
      .attr('stroke-width', 10)
      .attr('cursor', 'pointer')
      .selectAll('line')
      .data(simLinks)
      .join('line')
      .on('click', (ev, d) => {
        ev.stopPropagation();
        const source = linkEndId(d.source);
        const target = linkEndId(d.target);
        setSelectedNode(null);
        setSelectedEdge({
          source,
          target,
          edge_type: d.edge_type,
          event_time: d.event_time,
          direction: edgeDirectionRelativeToStart(source, target, startIdRef.current),
        });
      });
    hit.append('title').text((d) => {
      const dir = edgeDirectionRelativeToStart(
        linkEndId(d.source),
        linkEndId(d.target),
        startIdRef.current,
      );
      return (
        `${d.edge_type} — ${EDGE_MEANINGS[d.edge_type as EdgeType] ?? 'unknown edge type'}\n` +
        `${linkEndId(d.source)} → ${linkEndId(d.target)}\n` +
        `event time: ${d.event_time}` +
        (dir ? `\ndirection: ${dir} (relative to traversal start)` : '')
      );
    });

    const link = root
      .append('g')
      .attr('stroke', '#999')
      .attr('stroke-opacity', 0.6)
      .selectAll('line')
      .data(simLinks)
      .join('line')
      .attr('stroke-width', 1.5)
      .attr('stroke', (d) => EDGE_COLOR[d.edge_type as EdgeType] ?? '#999')
      .attr('marker-end', (d) => `url(#mk-arrow-${d.edge_type})`)
      .attr('pointer-events', 'none');

    const node = root
      .append('g')
      .attr('cursor', 'pointer')
      .selectAll<SVGGElement, SimNode>('g')
      .data(simNodes)
      .join('g')
      .call(drag(sim))
      .on('click', (ev, d) => {
        ev.stopPropagation();
        setSelectedEdge(null);
        setSelectedNode({
          id: d.id,
          kind: d.kind,
          label: d.label,
          unhydratable: d.unhydratable,
          record: d.record,
        });
      });

    node
      .append('circle')
      .attr('r', 12)
      .attr('fill', (d) => KIND_COLOR[d.kind])
      .attr('stroke', (d) => (d.unhydratable ? '#d62728' : '#fff'))
      .attr('stroke-width', (d) => (d.unhydratable ? 3 : 1.5))
      // Superseded facts get a dashed ring (see the legend).
      .attr('stroke-dasharray', (d) => (superseded.has(d.id) ? '4 3' : null));

    node
      .append('text')
      .attr('x', 16)
      .attr('y', 4)
      .attr('font-size', '11px')
      .attr('fill', 'currentColor')
      .attr('pointer-events', 'none')
      .text((d) => d.label);

    node.append('title').text((d) => `${d.kind}\n${d.id}`);

    sim.on('tick', () => {
      hit
        .attr('x1', (d) => (d.source as SimNode).x ?? 0)
        .attr('y1', (d) => (d.source as SimNode).y ?? 0)
        .attr('x2', (d) => (d.target as SimNode).x ?? 0)
        .attr('y2', (d) => (d.target as SimNode).y ?? 0);
      link
        .attr('x1', (d) => (d.source as SimNode).x ?? 0)
        .attr('y1', (d) => (d.source as SimNode).y ?? 0)
        .attr('x2', (d) => (d.target as SimNode).x ?? 0)
        .attr('y2', (d) => (d.target as SimNode).y ?? 0);
      node.attr('transform', (d) => `translate(${d.x ?? 0},${d.y ?? 0})`);
    });
  }

  /** Node-popup action (approved mock): restart the traversal from a node. */
  function traverseFromHere(n: GraphNode) {
    setStartKind(n.kind);
    setStartId(n.id);
    setSelectedNode(null);
    setSelectedEdge(null);
    void traverse({ start: { kind: n.kind, id: n.id } });
  }

  function drag(sim: d3.Simulation<SimNode, undefined>) {
    return d3
      .drag<SVGGElement, SimNode>()
      .on('start', (ev, d) => {
        if (!ev.active) sim.alphaTarget(0.3).restart();
        d.fx = d.x;
        d.fy = d.y;
      })
      .on('drag', (ev, d) => {
        d.fx = ev.x;
        d.fy = ev.y;
      })
      .on('end', (ev, d) => {
        if (!ev.active) sim.alphaTarget(0);
        d.fx = null;
        d.fy = null;
      });
  }

  return (
    <div className="memory-graph">
      <form className="graph-form" onSubmit={run}>
        <div className="filter-row">
          <label>
            start kind
            <select
              value={startKind}
              onChange={(e) => {
                const kind = e.target.value as NodeKind;
                setStartKind(kind);
                if (startValue.trim() !== '') setStartId(composeStartId(kind, startValue));
              }}
            >
              {NODE_KIND_ORDER.map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </label>
          <label>
            start value
            <input
              type="text"
              placeholder="e.g. memkeys"
              value={startValue}
              onChange={(e) => {
                const v = e.target.value;
                setStartValue(v);
                setStartId(composeStartId(startKind, v));
              }}
            />
          </label>
          <label className="grow">
            start id
            <input
              type="text"
              placeholder="auto-built: tenant|kind|value - kind uses dots (entity.project)"
              value={startId}
              onChange={(e) => setStartId(e.target.value)}
            />
          </label>
          <label>
            direction
            <select value={direction} onChange={(e) => setDirection(e.target.value as Direction)}>
              {DIRECTIONS.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
          </label>
          <label>
            depth
            <input
              type="number"
              min={1}
              max={10}
              value={depth}
              onChange={(e) => setDepth(Number(e.target.value) || 1)}
            />
          </label>
          <label className="checkbox">
            <input type="checkbox" checked={hydrate} onChange={(e) => setHydrate(e.target.checked)} />
            hydrate
          </label>
          <button type="submit" disabled={loading}>
            {loading ? 'Traversing…' : 'Traverse'}
          </button>
          <button type="button" className="link-button" onClick={runExample} disabled={loading}>
            Try example
          </button>
        </div>
        <div className="edge-filters">
          <span>edges:</span>
          {EDGE_TYPES.map((t) => (
            <label key={t} className="checkbox">
              <input type="checkbox" checked={edges.includes(t)} onChange={() => toggleEdge(t)} />
              {t}
            </label>
          ))}
          <span className="hint">(none = all)</span>
          <span className="hint">
            start id = tenant|kind|value · current tenant: {auth.tenantId.trim() || 'not set'}
          </span>
        </div>
      </form>

      {error && <ErrorBanner error={error} />}
      {status && <div className="graph-status">{status}</div>}

      <svg ref={svgRef} className="graph-canvas" />

      <GraphLegend />

      {(selectedNode || selectedEdge) && (
        <DetailPopup
          node={selectedNode}
          edge={selectedEdge}
          onTraverseFromHere={traverseFromHere}
          onClose={() => {
            setSelectedNode(null);
            setSelectedEdge(null);
          }}
        />
      )}
    </div>
  );
}
