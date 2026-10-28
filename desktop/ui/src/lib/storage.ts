// The Volumes and Networks sections' models (step 14.6b): the mock's
// `usersOfVol`, `membersOf`, `netsOf` and `hostPort`
// (docs/design/shipyard-ui.html), as plain functions over the API's
// volumes, networks and the map's nodes. A volume is used by every
// container that names it, running or not (the engine refuses to delete it
// while one does); a network's taps are the containers on it, each with its
// address and, from Shipyard's own view, the host's end of its veth pair.
import type { Network, NetworkView, StoredVolume, VolumesAnswer } from "./backend";
import { DEFAULT_NETWORK, isBridge, networkColors, type MapModel, type MapNode } from "./graph";
import { MB } from "./model";


/** A container that mounts a volume. */
export interface VolumeUser {
  node: MapNode;
  path: string;
  mode: "ro" | "rw";
}

/** A volume, as drawn. */
export interface VolumeView extends Omit<StoredVolume, "size"> {
  /** MB (at least this when not `complete`). */
  size: number;
  users: VolumeUser[];
}

/** The Volumes section's model. */
export interface VolumesModel {
  volumes: VolumeView[];
  /** The volumes no container names, and their MB (at least, when one
   * isn't walked whole: `unusedComplete` false). */
  unused: VolumeView[];
  unusedSize: number;
  unusedComplete: boolean;
  /** MB in every volume. */
  total: number;
  /** The largest volume's MB (a drum's fill is against it). */
  largest: number;
}

/** The model of `answer`'s volumes, their users from `nodes` (every
 * container, running or not). */
export function volumesModel(answer: VolumesAnswer, nodes: MapNode[]): VolumesModel {
  const volumes = answer.volumes.map((volume): VolumeView => ({
    ...volume,
    size: volume.size / MB,
    users: nodes.flatMap((node) =>
      node.mounts.filter((mount) => mount.vol === volume.name).map((mount) => ({ node, path: mount.path, mode: mount.mode })),
    ),
  }));
  const unused = volumes.filter((volume) => !volume.users.length);
  const sum = (some: VolumeView[]) => some.reduce((total, volume) => total + volume.size, 0);
  return {
    volumes,
    unused,
    unusedSize: sum(unused),
    unusedComplete: unused.every((volume) => volume.complete),
    total: sum(volumes),
    largest: Math.max(0, ...volumes.map((volume) => volume.size)),
  };
}

/** A drum's fill: `size` against `largest`, at most full (the drum draws a
 * sliver for the least). */
export function fillOf(size: number, largest: number): number {
  return largest > 0 ? Math.min(1, size / largest) : 0;
}

/** A container on a bridge network, as its band draws it. */
export interface Tap {
  node: MapNode;
  address: string;
  /** The host's end of its veth pair, where Shipyard's view says it. */
  veth: string | null;
  /** Another network it's on (the first), its tag. */
  also: { full: string; name: string; color: string } | null;
  /** A host port it publishes (the first), its tag on a network with a way
   * out. */
  hostPort: number | null;
}

/** A bridge network, as its band draws it. */
export interface Band {
  full: string;
  /** Compose's short name, else its name. */
  name: string;
  subnet: string | null;
  gateway: string | null;
  internal: boolean;
  color: string;
  /** Its bridge's name, where Shipyard's view says it. */
  bridge: string | null;
  /** Its DNS server, where Shipyard's view says it: none on the default
   * network. */
  dns: "running" | "stopped" | "none" | null;
  taps: Tap[];
  /** The running containers on it that are on other networks too: each is
   * a path between them. */
  bridging: { node: MapNode; others: string[] }[];
}

/** The Networks section's model: the bridge networks (the built-ins
 * aren't), their endpoints counted. */
export interface NetworksModel {
  bands: Band[];
  endpoints: number;
}

/** The model of `networks`' bridges, their taps from `model`'s containers,
 * their bridges and veths from `views` (by full name). */
export function networksModel(networks: Network[], model: MapModel, views: Map<string, NetworkView | null>): NetworksModel {
  const colors = networkColors(networks);
  const bridges = networks.filter(isBridge);
  const onBridge = new Set(bridges.map((network) => network.name));
  const bands = bridges.map((network): Band => {
    const view = views.get(network.name) ?? null;
    const members = model.containers.filter((node) => node.nets.some((net) => net.full === network.name));
    const othersOf = (node: MapNode) => node.nets.filter((net) => net.full !== network.name && onBridge.has(net.full));
    return {
      full: network.name,
      name: network.short,
      subnet: network.subnet,
      gateway: view?.gateway ?? network.gateway,
      internal: network.internal,
      color: colors.get(network.name) ?? "var(--faint)",
      bridge: view?.bridge ?? null,
      dns: !view ? null : network.name === DEFAULT_NETWORK ? "none" : view.dns_running ? "running" : "stopped",
      taps: members.map((node): Tap => {
        const other = othersOf(node)[0];
        // The live lease first: the list's address can lag a start.
        const endpoint = view?.endpoints.find((one) => one.container === node.id);
        return {
          node,
          address: endpoint?.address || node.nets.find((net) => net.full === network.name)?.ip || "",
          veth: endpoint?.host_veth ?? null,
          also: other
            ? {
                full: other.full,
                name: other.name,
                color: other.color,
              }
            : null,
          hostPort: model.entries.find((entry) => entry.to === node.id)?.port ?? null,
        };
      }),
      bridging: members
        .filter((node) => (node.state === "running" || node.state === "paused") && othersOf(node).length)
        .map((node) => ({
          node,
          others: othersOf(node).map((net) => net.name),
        })),
    };
  });
  const endpoints = model.containers.reduce((sum, node) => sum + node.nets.filter((net) => onBridge.has(net.full)).length, 0);
  return { bands, endpoints };
}
