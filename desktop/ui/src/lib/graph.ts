// The map's model (step 14.3): what the design's Map draws, made from the
// links answer (`/shipyard/links`), the container list and the networks.
// Plain functions, the mock's rules (docs/design/shipyard-ui.html's
// `graph`, `isUp`, `degraded`, `walk`, `affected`, `startOrder`), so each is
// tested without a page.
//
// A declared link (compose's `depends_on`, which has no port) and the
// traffic seen between the same two containers are one edge: solid, its
// port and its connections the traffic's. Traffic alone is the dotted
// "undeclared" edge. A declared link's own `refused` (its condition not met,
// or its service has no container) draws it red.
import type { Container, Entry, LinkEdge, Links, Network } from "./backend";
import { boxState, codeOf, type BoxState, type LinkNode } from "./model";

/** A section a node's chip or key opens (the volume or network named
 * there selected). */
export type Section = "images" | "volumes" | "networks";

/** A network of a container, as its node's livery shows it. */
export interface NodeNet {
  /** The network's name as shown (compose's short name). */
  name: string;
  /** Its full name. */
  full: string;
  ip: string;
  color: string;
}

/** A named volume mounted in a container. */
export interface NodeMount {
  vol: string;
  path: string;
  mode: "ro" | "rw";
}

/** A node of the map: a container, or a compose service with none. */
export interface MapNode {
  id: string;
  name: string;
  image: string;
  state: BoxState;
  /** A 40′ box: `memory.max` over 512 MiB, or none. */
  big: boolean;
  owner: string;
  serial: string;
  check: string;
  project: string | null;
  /** Its compose service, if it's one's. */
  service: string | null;
  /** Its compose file(s), as compose labels them (where a fix goes). */
  composeFile: string | null;
  nets: NodeNet[];
  mounts: NodeMount[];
  /** A compose service with no container (`missing`). */
  ghost: boolean;
}

/** An edge between two nodes. */
export interface MapEdge {
  from: string;
  to: string;
  /** `declared` (whether or not traffic was seen on it) or `observed`. */
  kind: "declared" | "observed";
  port: number | null;
  protocol: string | null;
  connections: number;
  /** `active`, `idle` (its source isn't up) or `broken` (refused). */
  state: "active" | "idle" | "broken";
}

/** A published port, the edge from the host to its container (one per
 * host port and protocol, whatever host addresses publish it). */
export interface MapEntry {
  to: string;
  port: number;
  protocol: string;
  connections: number;
  active: boolean;
}

/** A network some node on the map is on, for the key. */
export interface MapNetwork {
  name: string;
  full: string;
  subnet: string | null;
  color: string;
}

/** The map: its nodes (linked, then loose), edges, entries and facts. */
export interface MapModel {
  /** Every container, in the answer's order (the Yard's): no ghosts. */
  containers: MapNode[];
  linked: MapNode[];
  loose: MapNode[];
  /** The linked, then the loose: every node, ghosts too. */
  all: MapNode[];
  edges: MapEdge[];
  entries: MapEntry[];
  networks: MapNetwork[];
  /** Whether connections between containers are seen (root's networks). */
  observed: boolean;
  /** Why not, if they aren't. */
  unobserved: string | null;
  /** The nodes running. */
  up: Set<string>;
  /** Each node's health, if not fine (see [`health`]). */
  health: Map<string, "degraded" | "affected">;
}

/** The network colours, after the default network's (`--net-c`). */
const PALETTE = ["var(--net-a)", "var(--net-b)", "var(--net-d)", "var(--net-e)", "var(--net-f)", "var(--net-g)"];

/** Shipyard's default network: no DNS server on it, by rule. */
export const DEFAULT_NETWORK = "bridge";

/** Whether `network` is a bridge network (by its driver: the built-ins,
 * `host`, `none`, `pasta`, aren't). The one rule; the core counts the
 * rail's networks by the same. */
export function isBridge(network: Pick<Network, "driver">): boolean {
  return network.driver === "bridge";
}

/** Each network's colour, by its full name: the default network's
 * `--net-c`, the others the palette's in the API's order (cycled). */
export function networkColors(networks: Network[]): Map<string, string> {
  const colors = new Map<string, string>();
  let next = 0;
  for (const network of networks) {
    if (!isBridge(network)) continue;
    if (network.name === DEFAULT_NETWORK) {
      colors.set(network.name, "var(--net-c)");
    } else {
      colors.set(network.name, PALETTE[next % PALETTE.length] as string);
      next += 1;
    }
  }
  return colors;
}

/** The node of a container: its link node, its summary in the list. */
function containerNode(node: LinkNode, summary: Container | undefined, networks: Map<string, Network>, colors: Map<string, string>): MapNode {
  const { owner, serial, check } = codeOf(node.serial);
  const nets = Object.entries(summary?.NetworkSettings?.Networks ?? {}).map(([full, net]) => ({
    name: networks.get(full)?.short ?? full,
    full,
    ip: net.IPAddress ?? "",
    color: colors.get(full) ?? "var(--faint)",
  }));
  const mounts = (summary?.Mounts ?? [])
    .filter((mount) => mount.Type === "volume" && mount.Name)
    .map((mount) => ({ vol: mount.Name as string, path: mount.Destination ?? "", mode: mount.RW === false ? ("ro" as const) : ("rw" as const) }));
  return {
    id: node.id,
    name: node.name,
    image: summary?.Image ?? "",
    state: boxState({ state: summary?.State ?? node.state, exit_code: node.exit_code }),
    big: node.memory_max === null || node.memory_max > 512 * 1024 * 1024,
    owner,
    serial,
    check,
    project: node.project,
    service: node.service ?? null,
    composeFile: summary?.Labels?.["com.docker.compose.project.config_files"] ?? null,
    nets,
    mounts,
    ghost: false,
  };
}

/** The map's model. `containers` is the list (by ID): a node the list no
 * longer has (destroyed since the links were read) is left out, with its
 * edges. */
export function mapModel(links: Links, containers: Map<string, Container>, networks: Network[]): MapModel {
  const colors = networkColors(networks);
  const byName = new Map(networks.map((network) => [network.name, network]));
  const nodes: MapNode[] = (links.containers as LinkNode[])
    .filter((node) => containers.has(node.id))
    .map((node) => containerNode(node, containers.get(node.id), byName, colors));
  for (const missing of links.missing ?? []) {
    nodes.push({
      id: missing.id,
      name: missing.service,
      image: "no container",
      state: "exited",
      big: false,
      owner: "SHPU",
      serial: "",
      check: "",
      project: missing.project,
      service: missing.service,
      composeFile: null,
      nets: [],
      mounts: [],
      ghost: true,
    });
  }
  const known = new Set(nodes.map((node) => node.id));
  const up = new Set(nodes.filter((node) => node.state === "running").map((node) => node.id));
  const edges = mergeEdges(links.links.filter((link) => known.has(link.from) && known.has(link.to)), up);
  const entries: MapEntry[] = [];
  for (const entry of (links.entries ?? []).filter((entry: Entry) => known.has(entry.container))) {
    const same = entries.find((e) => e.to === entry.container && e.port === entry.host_port && e.protocol === entry.protocol);
    if (same) {
      // The same port on another host address (IPv4 and IPv6): one chip.
      same.connections = Math.max(same.connections, entry.connections ?? 0);
      continue;
    }
    entries.push({
      to: entry.container,
      port: entry.host_port,
      protocol: entry.protocol,
      connections: entry.connections ?? 0,
      active: up.has(entry.container),
    });
  }
  const linkedIds = new Set([...edges.flatMap((edge) => [edge.from, edge.to]), ...entries.map((entry) => entry.to)]);
  const linked = nodes.filter((node) => linkedIds.has(node.id));
  const loose = nodes.filter((node) => !linkedIds.has(node.id));
  const onMap = new Set(linked.flatMap((node) => node.nets.map((net) => net.full)));
  const model: MapModel = {
    containers: nodes.filter((node) => !node.ghost),
    linked,
    loose,
    all: [...linked, ...loose],
    edges,
    entries,
    networks: networks
      .filter((network) => onMap.has(network.name) && colors.has(network.name))
      .map((network) => ({
        name: network.short,
        full: network.name,
        subnet: network.subnet,
        color: colors.get(network.name) as string,
      })),
    observed: links.observed.available,
    unobserved: links.observed.available ? null : links.observed.reason,
    up,
    health: new Map(),
  };
  model.health = health(model);
  return model;
}

/** One edge per pair: a declared link with the traffic seen on it, else
 * the traffic alone; in the answer's order; none from a node to itself. */
export function mergeEdges(given: LinkEdge[], up: Set<string>): MapEdge[] {
  // A container's link to itself is no dependency to draw (and would push
  // it a column further on each pass of the layout's depth loop).
  const links = given.filter((link) => link.from !== link.to);
  const byPair = new Map<string, MapEdge & { refused: boolean }>();
  const key = (link: LinkEdge) => `${link.from}\u0000${link.to}`;
  for (const link of links) {
    if (link.kind !== "declared") continue;
    const known = byPair.get(key(link));
    if (known) {
      known.refused ||= link.refused;
      continue;
    }
    byPair.set(key(link), {
      from: link.from,
      to: link.to,
      kind: "declared",
      port: null,
      protocol: null,
      connections: 0,
      state: "active",
      refused: link.refused || link.to_missing,
    });
  }
  for (const link of links) {
    if (link.kind !== "observed") continue;
    let edge = byPair.get(key(link));
    if (!edge) {
      edge = {
        from: link.from,
        to: link.to,
        kind: "observed",
        port: null,
        protocol: null,
        connections: 0,
        state: "active",
        refused: false,
      };
      byPair.set(key(link), edge);
    }
    if (link.port !== null && (edge.port === null || link.port < edge.port)) {
      edge.port = link.port;
      edge.protocol = link.protocol;
    }
    edge.connections += link.connections;
  }
  return [...byPair.values()].map(({ refused, ...edge }) => ({
    ...edge,
    state: !up.has(edge.from) ? "idle" : refused ? "broken" : "active",
  }));
}

/** What `name` needs (`down`) or what needs it (`up`), through `edges`. */
export function walk(edges: MapEdge[], name: string, dir: "down" | "up"): Set<string> {
  const out = new Set<string>();
  const stack = [name];
  while (stack.length) {
    const n = stack.pop() as string;
    for (const edge of edges) {
      const [a, b] = dir === "down" ? [edge.from, edge.to] : [edge.to, edge.from];
      if (a === n && !out.has(b) && b !== name) {
        out.add(b);
        stack.push(b);
      }
    }
  }
  return out;
}

/** Each node's health: `degraded` (it runs, and something it needs is
 * refused), `affected` (it runs, and something further down is refused or
 * degraded), else nothing. */
export function health(model: Omit<MapModel, "health">): Map<string, "degraded" | "affected"> {
  const all = model.all;
  const isUp = (id: string) => model.up.has(id);
  const degraded = new Set(
    all
      .filter((node) => isUp(node.id) && model.edges.some((edge) => edge.from === node.id && edge.state === "broken"))
      .map((node) => node.id),
  );
  // Refused somewhere down: not a node that merely isn't running (a
  // finished compose job is no fault).
  const refused = new Set(model.edges.filter((edge) => edge.state === "broken").map((edge) => edge.to));
  const out = new Map<string, "degraded" | "affected">();
  for (const node of all) {
    if (degraded.has(node.id)) out.set(node.id, "degraded");
    else if (isUp(node.id) && [...walk(model.edges, node.id, "down")].some((n) => refused.has(n) || degraded.has(n))) {
      out.set(node.id, "affected");
    }
  }
  return out;
}

/** What the status line says: the nodes something up needs and is refused
 * (a stopped or failed container's start the fix), and the edges'
 * counts. */
export interface MapStatus {
  down: MapNode[];
  degraded: MapNode[];
  affected: MapNode[];
  /** Edges whose source is up. */
  active: number;
  /** Edges seen but not declared. */
  undeclared: number;
}

/** The status line's facts. */
export function mapStatus(model: MapModel): MapStatus {
  const all = model.all;
  const isUp = (id: string) => model.up.has(id);
  const states = model.health;
  return {
    // Refused, not merely not running: a compose job that finished (its
    // `service_completed_successfully` met) is no fault.
    down: all.filter((node) => model.edges.some((edge) => edge.to === node.id && edge.state === "broken")),
    degraded: all.filter((node) => states.get(node.id) === "degraded"),
    affected: all.filter((node) => states.get(node.id) === "affected"),
    active: model.edges.filter((edge) => isUp(edge.from)).length,
    undeclared: model.edges.filter((edge) => edge.kind === "observed").length,
  };
}

/** Each compose project's start order: tiers of names, what's needed
 * first, from declared links alone (the mock's `startOrder`). Projects
 * with no declared link are left out. */
export function startOrders(model: MapModel): { project: string; tiers: string[][] }[] {
  const all = model.all;
  const projects = [...new Set(all.map((node) => node.project).filter((p): p is string => p !== null))];
  const declared = model.edges.filter((edge) => edge.kind === "declared");
  return projects
    .map((project) => {
      const members = all.filter((node) => node.project === project);
      const height = new Map(members.map((node) => [node.id, 0]));
      const inProject = declared.filter((edge) => height.has(edge.from) && height.has(edge.to));
      for (let i = 0; i < members.length; i++) {
        for (const edge of inProject) {
          height.set(edge.from, Math.max(height.get(edge.from) ?? 0, (height.get(edge.to) ?? 0) + 1));
        }
      }
      const tiers: string[][] = [];
      for (const node of members) (tiers[height.get(node.id) ?? 0] ??= []).push(node.name);
      return { project, tiers: tiers.filter(Boolean), declared: inProject.length };
    })
    .filter((order) => order.declared > 0)
    .map(({ project, tiers }) => ({ project, tiers }));
}
