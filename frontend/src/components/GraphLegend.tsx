import { EDGE_COLOR, EDGE_LEGEND_ITEMS, KIND_COLOR, NODE_KIND_ORDER } from '../lib/legend';

/**
 * Static legend under the Memory Graph (ISI-6582): the closed edge-semantics
 * enum with meanings, node-kind colors, and the node style overrides —
 * dashed ring = superseded fact, red ring = unhydratable node.
 */
export default function GraphLegend() {
  return (
    <div className="graph-legend" data-testid="graph-legend">
      <div className="legend-col">
        <div className="legend-title">edge meanings</div>
        <ul className="legend-list">
          {EDGE_LEGEND_ITEMS.map((item) => (
            <li key={item.type} className="legend-item">
              <span className="legend-edge-swatch" style={{ background: EDGE_COLOR[item.type] }} />
              <span className="mono legend-edge-name">{item.type}</span>
              <span className="legend-meaning">{item.meaning}</span>
            </li>
          ))}
        </ul>
      </div>
      <div className="legend-col">
        <div className="legend-title">node kinds</div>
        <ul className="legend-list">
          {NODE_KIND_ORDER.map((k) => (
            <li key={k} className="legend-item">
              <span className="legend-swatch" style={{ background: KIND_COLOR[k] }} />
              <span className="mono">{k}</span>
            </li>
          ))}
        </ul>
        <ul className="legend-list legend-notes">
          <li className="legend-item">
            <span className="legend-swatch legend-swatch-dashed" />
            <span>superseded fact</span>
          </li>
          <li className="legend-item">
            <span className="legend-swatch legend-swatch-unhydratable" />
            <span>unhydratable node</span>
          </li>
        </ul>
      </div>
    </div>
  );
}
