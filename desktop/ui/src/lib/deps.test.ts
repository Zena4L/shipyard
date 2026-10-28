import { describe, expect, it } from "vitest";
import type { Container, LinkEdge, Links } from "./backend";
import { dependsOnYaml, hintsFor, hostNeeds, impact, subtree } from "./deps";
import { mapModel, type MapModel } from "./graph";

const link = (from: string, to: string, kind: LinkEdge["kind"], extra: Partial<LinkEdge> = {}): LinkEdge => ({
  from,
  to,
  to_missing: false,
  kind,
  port: kind === "observed" ? 5432 : null,
  protocol: kind === "observed" ? "tcp" : null,
  condition: null,
  connections: 1,
  via_host_port: false,
  refused: false,
  ...extra,
});

const node = (id: string, state = "running", exit_code: number | null = null) => ({
  id,
  name: id,
  serial: "SHPU 000001 0",
  memory_max: null,
  started_at: null,
  project: "shop",
  service: `${id}-svc`,
  state,
  exit_code,
});

/** web → api → db (declared), worker → db (seen, undeclared); web
 * publishes 8080; `states` overrides a node's state, `codes` its exit. */
function model(states: Record<string, string> = {}, refused = false, codes: Record<string, number> = {}): MapModel {
  const ids = ["web", "api", "db", "worker"];
  const links: Links = {
    version: 1,
    containers: ids.map((id) => node(id, states[id] ?? "running", codes[id] ?? null)),
    entries: [{ container: "web", host_ip: "0.0.0.0", host_port: 8080, container_port: 80, protocol: "tcp" }],
    links: [link("web", "api", "declared"), link("api", "db", "declared", { refused }), link("worker", "db", "observed")],
    observed: { available: true, cause: null, reason: null },
  };
  const containers = new Map<string, Container>(ids.map((id) => [id, { Id: id, State: states[id] ?? "running" }]));
  return mapModel(links, containers, []);
}

describe("the dependency trees", () => {
  it("says what a container needs and what needs it, each node once along a path", () => {
    const m = model();
    const down = subtree(m, "web", "down");
    expect(down.map((b) => b.node.id)).toEqual(["api"]);
    expect(down[0]?.children.map((b) => b.node.id)).toEqual(["db"]);
    const up = subtree(m, "db", "up");
    expect(up.map((b) => [b.node.id, b.meta, b.kind])).toEqual([
      ["api", "", "declared"],
      ["worker", "tcp 5432 · undeclared", "observed"],
    ]);
    expect(hostNeeds(m, "web")).toEqual([{ port: 8080 }]);
    expect(hostNeeds(m, "api")).toEqual([]);
  });

  it("says a refused need refused", () => {
    const m = model({ db: "exited" }, true);
    expect(subtree(m, "api", "down")[0]).toMatchObject({ meta: "refused", kind: "broken" });
  });
});

describe("the impact line", () => {
  it("says who a stopped container degrades, who'd lose a running one, and a contained failure", () => {
    const down = model({ db: "exited" }, true);
    // Both need it: api by its declaration, worker by its connection.
    expect(impact(down, down.containers[2]!)).toEqual({ kind: "down", direct: ["api", "worker"] });
    const m = model();
    expect(impact(m, m.containers[1]!)).toEqual({ kind: "if-stops", direct: ["web"], indirect: [] });
    expect(impact(m, m.containers[2]!)).toEqual({ kind: "if-stops", direct: ["api", "worker"], indirect: ["web"] });
    const stopped = model({ worker: "exited" });
    expect(impact(stopped, stopped.containers[3]!)).toEqual({ kind: "contained", failed: false });
    const failed = model({ worker: "exited" }, false, { worker: 1 });
    expect(impact(failed, failed.containers[3]!)).toEqual({ kind: "contained", failed: true });
  });
});

describe("the hints", () => {
  it("offers the depends_on lines for a connection compose doesn't declare", () => {
    const m = model();
    const hints = hintsFor(m, m.containers[3]!);
    expect(hints.map((h) => [h.to.id, h.port, h.service, h.target])).toEqual([["db", 5432, "worker-svc", "db-svc"]]);
    expect(hintsFor(m, m.containers[1]!)).toEqual([]);
  });

  it("writes the lines compose reads, a name YAML would misread quoted", () => {
    expect(dependsOnYaml("worker", "db")).toBe(
      "services:\n  worker:\n    depends_on:\n      db:\n        condition: service_started\n",
    );
    expect(dependsOnYaml("my app", "yes: no")).toContain('"my app":');
    expect(dependsOnYaml("my app", "yes: no")).toContain('"yes: no":');
    // Plain names YAML reads as a null, a boolean or a number.
    for (const word of ["null", "NULL", "true", "No", "on", "y", "123", "1e3", "0x1f", ".inf"]) {
      expect(dependsOnYaml("web", word)).toContain(`      "${word}":\n`);
    }
    // A line break YAML has and JSON doesn't escape.
    expect(dependsOnYaml("web", "a\u2028b")).toContain('"a\\u2028b":');
  });

  it("offers no fix for a target outside the service's compose project", () => {
    const m = model();
    const worker = m.containers[3]!;
    const elsewhere = { ...m, all: m.all.map((n) => (n.id === "db" ? { ...n, project: "other" } : n)) };
    expect(hintsFor(elsewhere, worker)).toMatchObject([{ service: null }]);
    const standalone = { ...m, all: m.all.map((n) => (n.id === "db" ? { ...n, project: null } : n)) };
    expect(hintsFor(standalone, worker)).toMatchObject([{ service: null }]);
  });
});
