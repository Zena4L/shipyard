// The map's layout (step 14.3): the mock's `renderMap` geometry
// (docs/design/shipyard-ui.html), number for number, in plain functions.
// Columns by the longest path from the entry points, each column after the
// first ordered by its nodes' sources' places (their barycentre), widths by
// box size (20′ or 40′), host port chips beside their container and kept
// apart, edges spread where several meet one node, each a cubic curve.
import type { MapEdge, MapModel, MapNode } from "./graph";

/** Which overlays are drawn above and below each node. */
export interface Overlays {
  networks: boolean;
  volumes: boolean;
  images: boolean;
}

/** A node's place. */
export interface Place {
  x: number;
  y: number;
  w: number;
  /** Its box's middle, where edges meet it. */
  cy: number;
  /** How wide its volume chips may grow. */
  maxw: number;
}

/** A host port's chip, at the left. */
export interface Chip {
  to: string;
  port: number;
  protocol: string;
  y: number;
  /** Unique on the map (a port may be published for TCP and UDP). */
  key: string;
}

/** An edge laid out: a link's or a host port's. */
export interface LaidEdge {
  /** Unique on the map. */
  key: string;
  host: boolean;
  from: string;
  to: string;
  port: number | null;
  edge: MapEdge | null;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  /** `active`, `idle` or `broken`. */
  state: "active" | "idle" | "broken";
  /** Its connections (the dots' source). */
  connections: number;
  /** The SVG path. */
  d: string;
}

/** The whole map's geometry. */
export interface Layout {
  width: number;
  height: number;
  places: Map<string, Place>;
  chips: Chip[];
  edges: LaidEdge[];
  /** Where the first column of containers starts. */
  firstColumn: number;
}

/** A box's height. */
export const NODE_H = 74;
/** The space between two boxes in a column. */
export const VGAP = 26;
/** Where the first box may start (below the column heads). */
export const TOP = 32;
/** The host column's width. */
export const HOSTW = 76;
/** The narrowest gap between two columns. */
export const MINGAP = 52;

/** `edges` but those that close a cycle (found walking from each node in
 * turn, depth first): two services that call each other are laid out as
 * one after the other, not pushed further at each pass. */
export function forward(nodes: MapNode[], edges: MapEdge[]): MapEdge[] {
  const state = new Map<string, "open" | "done">();
  const back = new Set<MapEdge>();
  const visit = (id: string) => {
    state.set(id, "open");
    for (const edge of edges) {
      if (edge.from !== id) continue;
      const next = state.get(edge.to);
      if (next === "open") back.add(edge);
      else if (next === undefined) visit(edge.to);
    }
    state.set(id, "done");
  };
  for (const node of nodes) if (!state.has(node.id)) visit(node.id);
  return edges.filter((edge) => !back.has(edge));
}

/** Each linked node's column: its longest path from a source, cycles
 * broken ([`forward`]). */
export function depths(nodes: MapNode[], edges: MapEdge[]): Map<string, number> {
  const acyclic = forward(nodes, edges);
  const depth = new Map(nodes.map((node) => [node.id, 0]));
  for (let i = 0; i < nodes.length; i++) {
    for (const edge of acyclic) {
      depth.set(edge.to, Math.max(depth.get(edge.to) ?? 0, (depth.get(edge.from) ?? 0) + 1));
    }
  }
  return depth;
}

/** The columns, each after the first ordered by its nodes' sources'
 * average place (a node with no source last). */
export function columns(nodes: MapNode[], given: MapEdge[]): MapNode[][] {
  const edges = forward(nodes, given);
  const depth = depths(nodes, edges);
  const cols: MapNode[][] = [];
  for (const node of nodes) (cols[depth.get(node.id) ?? 0] ??= []).push(node);
  const index = new Map<string, number>();
  cols.forEach((col) => col.forEach((node, i) => index.set(node.id, i)));
  const bary = (node: MapNode) => {
    const sources = edges.filter((edge) => edge.to === node.id).map((edge) => index.get(edge.from) ?? 0);
    return sources.length ? sources.reduce((a, b) => a + b, 0) / sources.length : 99;
  };
  for (let d = 1; d < cols.length; d++) {
    const col = cols[d];
    if (!col) continue;
    col.sort((a, b) => bary(a) - bary(b));
    col.forEach((node, i) => index.set(node.id, i));
  }
  return cols.filter(Boolean);
}

/** The map's geometry for `model`, `overlays` drawn, `avail` pixels wide. */
export function layout(model: MapModel, overlays: Overlays, avail: number): Layout {
  const linkEdges = model.edges;
  const cols = columns(model.linked, linkEdges);
  const topX = (overlays.images ? 18 : 0) + (overlays.networks ? 8 : 0);
  const botX = (node: MapNode) => (overlays.volumes && node.mounts.length ? 6 + node.mounts.length * 24 : 0);
  const height = (node: MapNode) => topX + NODE_H + botX(node);
  const w = (node: MapNode) => (node.big ? 180 : 90);
  const colW = cols.map((col) => Math.max(...col.map(w)));
  const sumW = colW.reduce((a, b) => a + b, 0);
  const need = HOSTW + MINGAP + sumW + MINGAP * Math.max(0, cols.length - 1);
  const width = Math.max(need, avail);
  const gap = cols.length > 1 ? Math.max(MINGAP, (width - HOSTW - MINGAP - sumW) / (cols.length - 1)) : MINGAP;
  const colHs = cols.map((col) => col.reduce((a, node) => a + height(node), 0) + VGAP * (col.length - 1));
  const innerH = Math.max(NODE_H, ...colHs);
  const places = new Map<string, Place>();
  let x = HOSTW + MINGAP;
  cols.forEach((col, d) => {
    let y = TOP + (innerH - (colHs[d] ?? 0)) / 2;
    for (const node of col) {
      places.set(node.id, { x, y, w: w(node), cy: y + topX + NODE_H / 2, maxw: w(node) + Math.min(gap - 16, 40) });
      y += height(node) + VGAP;
    }
    x += (colW[d] ?? 0) + gap;
  });

  // Host port chips, beside their container and kept apart.
  const chips: Chip[] = model.entries
    .filter((entry) => places.has(entry.to))
    .map((entry) => ({
      to: entry.to,
      port: entry.port,
      protocol: entry.protocol,
      y: (places.get(entry.to) as Place).cy - 13,
      key: `host>${entry.to}>${entry.port}/${entry.protocol}`,
    }))
    .sort((a, b) => a.y - b.y);
  chips.forEach((chip, i) => {
    const before = chips[i - 1];
    if (before && chip.y < before.y + 32) chip.y = before.y + 32;
  });

  const place = (id: string) => places.get(id) as Place;
  const entryOf = (chip: Chip) =>
    model.entries.find((entry) => entry.to === chip.to && entry.port === chip.port && entry.protocol === chip.protocol);
  const edges: LaidEdge[] = [
    ...chips.map((chip) => {
      const entry = entryOf(chip);
      return {
        key: chip.key,
        host: true,
        from: "host",
        to: chip.to,
        port: chip.port,
        edge: null,
        x1: HOSTW,
        y1: chip.y + 13,
        x2: 0,
        y2: 0,
        state: entry?.active ? ("active" as const) : ("idle" as const),
        connections: entry?.connections ?? 0,
        d: "",
      };
    }),
    ...linkEdges.map((edge) => ({
      key: `${edge.from}>${edge.to}`,
      host: false,
      from: edge.from,
      to: edge.to,
      port: edge.port,
      edge,
      x1: place(edge.from).x + place(edge.from).w,
      y1: place(edge.from).cy,
      x2: 0,
      y2: 0,
      state: edge.state,
      connections: edge.connections,
      d: "",
    })),
  ];
  for (const edge of edges) {
    edge.x2 = place(edge.to).x;
    edge.y2 = place(edge.to).cy;
  }
  spread(edges, "to");
  spread(edges, "from");
  for (const edge of edges) {
    const dx = Math.max(28, (edge.x2 - edge.x1) * 0.5);
    edge.d = `M${edge.x1},${edge.y1} C${edge.x1 + dx},${edge.y1} ${edge.x2 - dx},${edge.y2} ${edge.x2 - 2},${edge.y2}`;
  }
  return { width, height: TOP + innerH + 8, places, chips, edges, firstColumn: HOSTW + MINGAP };
}

/** The most dots on the map at once. */
export const MAX_DOTS = 200;

/** Dots a second on `edge`: none unless it's active with open connections,
 * more with more (by their logarithm), at most 2.4. */
export function dotRate(edge: Pick<LaidEdge, "state" | "connections">): number {
  if (edge.state !== "active" || edge.connections <= 0) return 0;
  return Math.min(2.4, 0.3 + 0.5 * Math.log2(1 + edge.connections));
}

/** Edges that meet one node, 9 px apart there, in the order of their
 * other ends. */
function spread(edges: LaidEdge[], key: "to" | "from") {
  const groups = new Map<string, LaidEdge[]>();
  for (const edge of edges) {
    const group = groups.get(edge[key]) ?? [];
    group.push(edge);
    groups.set(edge[key], group);
  }
  for (const group of groups.values()) {
    group.sort((a, b) => (key === "to" ? a.y1 - b.y1 : a.y2 - b.y2));
    group.forEach((edge, i) => {
      const shift = (i - (group.length - 1) / 2) * 9;
      if (key === "to") edge.y2 += shift;
      else edge.y1 += shift;
    });
  }
}
