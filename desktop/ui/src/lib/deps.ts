// The inspector's Dependencies (step 14.4a): the mock's `depsHTML`, `subtree`
// and `tnode` (docs/design/shipyard-ui.html) as plain functions over the
// map's model: what a container needs and what needs it, as trees (a
// published port is the host's need of it), the sentence that says what a
// stop would do, and a hint for each connection it makes that compose
// doesn't declare, with the lines to add (D14-4: the YAML to copy, never
// an edit of the user's file).
import type { MapEdge, MapModel, MapNode } from "./graph";
import { walk } from "./graph";

/** A branch of a tree: the node it reaches, how, and what's below it. */
export interface Branch {
  node: MapNode;
  /** What the edge says: its port, undeclared, refused. */
  meta: string;
  kind: "declared" | "observed" | "broken";
  children: Branch[];
}

/** A published port, at the top of a Needed by tree. */
export interface HostNeed {
  port: number;
}

/** What `edge` says in a tree (`tcp 5432 · undeclared`, `refused`). */
function metaOf(edge: MapEdge, dir: "down" | "up"): { meta: string; kind: Branch["kind"] } {
  const broken = dir === "down" && edge.state === "broken";
  const parts = [
    edge.port === null ? null : `${edge.protocol ?? "tcp"} ${edge.port}`,
    edge.kind === "observed" ? "undeclared" : null,
    broken ? "refused" : null,
  ].filter((part): part is string => part !== null);
  return { meta: parts.join(" · "), kind: broken ? "broken" : edge.kind };
}

/** The tree below `id`: what it needs (`down`) or what needs it (`up`),
 * each node once along a path. */
export function subtree(model: MapModel, id: string, dir: "down" | "up", seen: Set<string> = new Set([id])): Branch[] {
  return model.edges
    .filter((edge) => (dir === "down" ? edge.from : edge.to) === id)
    .flatMap((edge) => {
      const other = dir === "down" ? edge.to : edge.from;
      const node = model.all.find((n) => n.id === other);
      if (!node) return [];
      return [
        {
          node,
          ...metaOf(edge, dir),
          children: seen.has(other) ? [] : subtree(model, other, dir, new Set([...seen, other])),
        },
      ];
    });
}

/** The ports `id` publishes: the host's needs of it. */
export function hostNeeds(model: MapModel, id: string): HostNeed[] {
  return model.entries.filter((entry) => entry.to === id).map((entry) => ({ port: entry.port }));
}

/** What the inspector's impact line says, if anything. */
export type Impact =
  | { kind: "down"; direct: string[] }
  | { kind: "if-stops"; direct: string[]; indirect: string[] }
  | { kind: "contained"; failed: boolean }
  | null;

/** The mock's impact sentence: a container down that running ones need
 * says who's degraded; a running one that others need says who'd lose it;
 * one that needs others but that nothing needs says it's contained. */
export function impact(model: MapModel, node: MapNode): Impact {
  const up = (id: string) => model.up.has(id);
  const name = (id: string) => model.all.find((n) => n.id === id)?.name ?? id;
  const needers = [...walk(model.edges, node.id, "up")].filter(up);
  const direct = model.edges.filter((edge) => edge.to === node.id && up(edge.from)).map((edge) => edge.from);
  const indirect = needers.filter((id) => !direct.includes(id));
  if (!up(node.id) && direct.length) return { kind: "down", direct: direct.map(name) };
  if (up(node.id) && needers.length) return { kind: "if-stops", direct: direct.map(name), indirect: indirect.map(name) };
  if (!needers.length && model.edges.some((edge) => edge.from === node.id)) {
    return { kind: "contained", failed: node.state === "failed" };
  }
  return null;
}

/** A connection a container makes that compose doesn't declare. */
export interface Hint {
  to: MapNode;
  port: number | null;
  /** The container's compose service: the fix goes there (none, or a
   * target outside its compose project: nothing compose can declare, no
   * fix offered). */
  service: string | null;
  /** The target's name in the fix: its compose service, else its name. */
  target: string;
}

/** The hints for `node`: its undeclared connections. */
export function hintsFor(model: MapModel, node: MapNode): Hint[] {
  return model.edges
    .filter((edge) => edge.from === node.id && edge.kind === "observed")
    .flatMap((edge) => {
      const to = model.all.find((n) => n.id === edge.to);
      if (!to) return [];
      // A service another project's (or none's) isn't one this file names.
      const declarable = node.project !== null && to.project === node.project;
      return [{ to, port: edge.port, service: declarable ? node.service : null, target: to.service ?? to.name }];
    });
}

/** Names YAML (1.1 or 1.2) reads as a null or a boolean when plain. */
const YAML_WORDS = /^(?:~|null|true|false|yes|no|on|off|y|n)$/i;

/** `name` as a YAML key: plain when it starts with a letter or `_` and
 * isn't one of YAML's words (a number starts with neither), else quoted,
 * with the line breaks YAML has beyond JSON's escaped too. */
function yamlKey(name: string): string {
  if (/^[A-Za-z_][A-Za-z0-9_.-]*$/.test(name) && !YAML_WORDS.test(name)) return name;
  return JSON.stringify(name).replace(/[\u0085\u2028\u2029]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`);
}

/** The lines to add to compose's file for `service` to wait for `target`
 * (its start; a health check's `service_healthy` is the user's to choose). */
export function dependsOnYaml(service: string, target: string): string {
  return `services:\n  ${yamlKey(service)}:\n    depends_on:\n      ${yamlKey(target)}:\n        condition: service_started\n`;
}
