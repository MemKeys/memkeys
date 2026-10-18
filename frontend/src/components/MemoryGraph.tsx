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
import { GraphLink, GraphNode, toGraphData } from '../lib/transform';
import ErrorBanner from './ErrorBanner';

const NODE_KINDS: NodeKind[] = [
  'fact',
  'entity_project',
  'entity_issue',
  'entity_pr',
  'entity_agent',
  'entity_run',
  'a2a_hint',
];
const EDGE_TYPES: EdgeType[] = ['entity_fact', 'supersedes', 'produced_by', 'a2a_participant'];
const DIRECTIONS: Direction[] = ['out', 'in', 'both'];

// Stable color per node kind (color-blind-safe-ish categorical ramp).
const KIND_COLOR: Record<NodeKind, string> = {
  fact: '#4f8cff',
  entity_project: '#2ca02c',
  entity_issue: '#ff7f0e',
  entity_pr: '#9467bd',
  entity_agent: '#17becf',
  entity_run: '#bcbd22',
  a2a_hint: '#e377c2',
};

type SimNode = GraphNode & d3.SimulationNodeDatum;
type SimLink = d3.SimulationLinkDatum<SimNode> & { edge_type: string };

/**
 * Memory Graph — renders the neighborhood returned by
 * `POST /memory/graph/traverse` as a d3 force-directed graph. Topology-only or
 * unhydratable nodes are still drawn (body omitted, never faked); dropped
 * dangling edges are surfaced in the status line.
 */
export default function MemoryGraph() {
  const { auth } = useAuth();
  const svgRef = useRef<SVGSVGElement | null>(null);
  const [startKind, setStartKind] = useState<NodeKind>('fact');
  const [startId, setStartId] = useState('');
  const [direction, setDirection] = useState<Direction>('out');
  const [depth, setDepth] = useState(2);
  const [hydrate, setHydrate] = useState(true);
  const [edges, setEdges] = useState<EdgeType[]>([]);
  const [error, setError] = useState<APIError | null>(null);
  const [loading, setLoading] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  const toggleEdge = (t: EdgeType) =>
    setEdges((cur) => (cur.includes(t) ? cur.filter((x) => x !== t) : [...cur, t]));

  async function run(e: React.FormEvent) {
    e.preventDefault();
    if (startId.trim() === '') {
      setError({ error: 'A start node id is required.' });
      return;
    }
    setLoading(true);
    setError(null);
    const req: TraverseRequest = {
      start: { kind: startKind, id: startId.trim() },
      edges, // empty = all edge types
      direction,
      depth,
      hydrate,
      deadline_ms: 2000,
    };
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

    const root = svg.append('g');
    svg.call(
      d3
        .zoom<SVGSVGElement, unknown>()
        .scaleExtent([0.2, 4])
        .on('zoom', (ev) => root.attr('transform', ev.transform.toString())),
    );

    const link = root
      .append('g')
      .attr('stroke', '#999')
      .attr('stroke-opacity', 0.6)
      .selectAll('line')
      .data(simLinks)
      .join('line')
      .attr('stroke-width', 1.5);

    link.append('title').text((d) => d.edge_type);

    const node = root
      .append('g')
      .selectAll<SVGGElement, SimNode>('g')
      .data(simNodes)
      .join('g')
      .call(drag(sim));

    node
      .append('circle')
      .attr('r', 12)
      .attr('fill', (d) => KIND_COLOR[d.kind])
      .attr('stroke', (d) => (d.unhydratable ? '#d62728' : '#fff'))
      .attr('stroke-width', (d) => (d.unhydratable ? 3 : 1.5));

    node
      .append('text')
      .attr('x', 16)
      .attr('y', 4)
      .attr('font-size', '11px')
      .attr('fill', 'currentColor')
      .text((d) => d.label);

    node.append('title').text((d) => `${d.kind}\n${d.id}`);

    sim.on('tick', () => {
      link
        .attr('x1', (d) => (d.source as SimNode).x ?? 0)
        .attr('y1', (d) => (d.source as SimNode).y ?? 0)
        .attr('x2', (d) => (d.target as SimNode).x ?? 0)
        .attr('y2', (d) => (d.target as SimNode).y ?? 0);
      node.attr('transform', (d) => `translate(${d.x ?? 0},${d.y ?? 0})`);
    });
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
            <select value={startKind} onChange={(e) => setStartKind(e.target.value as NodeKind)}>
              {NODE_KINDS.map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </label>
          <label className="grow">
            start id
            <input
              type="text"
              placeholder="opaque node id"
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
        </div>
      </form>

      {error && <ErrorBanner error={error} />}
      {status && <div className="graph-status">{status}</div>}

      <svg ref={svgRef} className="graph-canvas" />
    </div>
  );
}
