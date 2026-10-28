import { describe, expect, it } from "vitest";
import type { Container, LinkEdge, Links, Network } from "./backend";
import { health, mapModel, mapStatus, mergeEdges, networkColors, startOrders, walk } from "./graph";
import { columns, depths, dotRate, forward, layout } from "./layout";

const link = (from: string, to: string, kind: LinkEdge["kind"], extra: Partial<LinkEdge> = {}): LinkEdge => ({
  from,
  to,
  to_missing: false,
  kind,
  port: kind === "observed" ? 80 : null,
  protocol: kind === "observed" ? "tcp" : null,
  condition: null,
  connections: kind === "observed" ? 1 : 0,
  via_host_port: false,
  refused: false,
  ...extra,
});

const node = (id: string, state = "running", project: string | null = "p", memory_max: number | null = 256 * 1024 * 1024) => ({
  id,
  name: id,
  serial: "SHPU 000001 0",
  memory_max,
  started_at: null,
  project,
  state,
  exit_code: state === "exited" ? 0 : null,
});

const listed = (...ids: string[]) =>
  new Map<string, Container>(ids.map((id) => [id, { Id: id, Image: `${id}:1` }]));

const answer = (nodes: ReturnType<typeof node>[], links: LinkEdge[], extra: Partial<Links> = {}): Links => ({
  version: 1,
  containers: nodes,
  links,
  observed: { available: true, cause: null, reason: null },
  ...extra,
});

describe("mergeEdges", () => {
  it("makes a declared link and its traffic one edge, its port and connections the traffic's", () => {
    const edges = mergeEdges(
      [link("a", "b", "declared"), link("a", "b", "observed", { port: 5432, connections: 3 }), link("a", "b", "observed", { port: 8080, connections: 2 })],
      new Set(["a", "b"]),
    );
    expect(edges).toHaveLength(1);
    expect(edges[0]).toMatchObject({ kind: "declared", port: 5432, connections: 5, state: "active" });
  });

  it("draws no edge from a node to itself", () => {
    expect(mergeEdges([link("a", "a", "declared"), link("a", "a", "observed")], new Set(["a"]))).toEqual([]);
  });

  it("draws traffic alone as undeclared, a refused link broken, an edge from a stopped source idle", () => {
    const edges = mergeEdges(
      [link("a", "b", "observed"), link("a", "c", "declared", { refused: true }), link("d", "b", "declared")],
      new Set(["a", "b"]),
    );
    expect(edges.map((edge) => [edge.kind, edge.state])).toEqual([
      ["declared", "broken"],
      ["declared", "idle"],
      ["observed", "active"],
    ]);
  });
});

describe("mapModel", () => {
  it("keeps containers the list has, a missing service as a ghost, and the loose apart", () => {
    const model = mapModel(
      answer([node("a"), node("b"), node("gone"), node("alone")], [link("a", "b", "declared"), link("a", "p/db", "declared", { to_missing: true, refused: true })], {
        missing: [{ id: "p/db", project: "p", service: "db" }],
      }),
      listed("a", "b", "alone"),
      [],
    );
    expect(model.linked.map((n) => n.id)).toEqual(["a", "b", "p/db"]);
    expect(model.linked.find((n) => n.id === "p/db")?.ghost).toBe(true);
    expect(model.loose.map((n) => n.id)).toEqual(["alone"]);
    expect(model.edges.find((edge) => edge.to === "p/db")?.state).toBe("broken");
  });

  it("names a compose network by its short name, coloured in the API's order, the default network grey", () => {
    const networks: Network[] = [
      { name: "p_front", short: "front", subnet: "10.1.0.0/24", internal: false, gateway: null, driver: "bridge" },
      { name: "bridge", short: "bridge", subnet: "10.88.0.0/16", internal: false, gateway: null, driver: "bridge" },
      { name: "p_back", short: "back", subnet: null, internal: true, gateway: null, driver: "bridge" },
      { name: "host", short: "host", subnet: null, internal: false, gateway: null, driver: "host" },
      // A network is a bridge by its driver, never its name: rootless's pasta.
      { name: "pasta", short: "pasta", subnet: null, internal: false, gateway: null, driver: "pasta" },
    ];
    const colors = networkColors(networks);
    expect([...colors]).toEqual([
      ["p_front", "var(--net-a)"],
      ["bridge", "var(--net-c)"],
      ["p_back", "var(--net-b)"],
    ]);
    const containers = listed("a");
    (containers.get("a") as Container).NetworkSettings = { Networks: { p_front: { IPAddress: "10.1.0.2" } } };
    const model = mapModel(answer([node("a")], [], { entries: [{ container: "a", host_ip: "0.0.0.0", host_port: 80, container_port: 80, protocol: "tcp" }] }), containers, networks);
    expect(model.linked[0]?.nets).toEqual([{ name: "front", full: "p_front", ip: "10.1.0.2", color: "var(--net-a)" }]);
    expect(model.networks.map((network) => network.name)).toEqual(["front"]);
    expect(model.entries[0]?.connections).toBe(0);
  });

  it("draws a box 40′ for a limit over 512 MiB, or none", () => {
    const model = mapModel(
      answer([node("small"), node("big", "running", "p", 1024 * 1024 * 1024), node("free", "running", "p", null)], [link("small", "big", "declared"), link("small", "free", "declared")]),
      listed("small", "big", "free"),
      [],
    );
    expect(model.linked.map((n) => n.big)).toEqual([false, true, true]);
  });
});

describe("health and the status line", () => {
  it("says what's refused, who's degraded by it and who's affected through them", () => {
    const model = mapModel(
      answer([node("web"), node("api"), node("db", "exited")], [link("web", "api", "declared"), link("api", "db", "declared", { refused: true })]),
      listed("web", "api", "db"),
      [],
    );
    expect([...health(model)]).toEqual([
      ["web", "affected"],
      ["api", "degraded"],
    ]);
    const status = mapStatus(model);
    expect(status.down.map((n) => n.id)).toEqual(["db"]);
    expect(status.degraded.map((n) => n.id)).toEqual(["api"]);
    expect(status.affected.map((n) => n.id)).toEqual(["web"]);
  });

  it("counts what's seen but not declared", () => {
    const model = mapModel(
      answer([node("a"), node("b"), node("c")], [link("a", "b", "declared"), link("a", "b", "observed"), link("a", "c", "observed")]),
      listed("a", "b", "c"),
      [],
    );
    expect(mapStatus(model)).toMatchObject({ active: 2, undeclared: 1 });
  });

  it("calls a finished job no fault, and counts the undeclared", () => {
    const model = mapModel(
      answer([node("api"), node("migrate", "exited")], [link("api", "migrate", "declared"), link("api", "migrate", "observed")]),
      listed("api", "migrate"),
      [],
    );
    const status = mapStatus(model);
    expect(status.down).toEqual([]);
    expect(health(model).size).toBe(0);
    expect(status.active).toBe(1);
    expect(status.undeclared).toBe(0);
  });

  it("walks what a node needs, and what needs it", () => {
    const edges = mergeEdges([link("a", "b", "declared"), link("b", "c", "declared"), link("d", "a", "declared")], new Set(["a", "b", "c", "d"]));
    expect([...walk(edges, "a", "down")]).toEqual(["b", "c"]);
    expect([...walk(edges, "a", "up")]).toEqual(["d"]);
  });

  it("orders each project's start by its declared links alone", () => {
    const model = mapModel(
      answer([node("web"), node("api"), node("db"), node("other", "running", "q")], [link("web", "api", "declared"), link("api", "db", "declared"), link("web", "db", "observed")]),
      listed("web", "api", "db", "other"),
      [],
    );
    expect(startOrders(model)).toEqual([{ project: "p", tiers: [["db"], ["api"], ["web"]] }]);
  });
});

describe("entries", () => {
  const entry = (port: number, protocol: string, host_ip = "0.0.0.0", connections = 0) => ({
    container: "a",
    host_ip,
    host_port: port,
    container_port: port,
    protocol,
    connections,
  });

  it("draws a port published for TCP and UDP as two chips, on two addresses as one", () => {
    const model = mapModel(
      answer([node("a")], [], { entries: [entry(53, "tcp"), entry(53, "udp"), entry(80, "tcp", "0.0.0.0", 1), entry(80, "tcp", "::", 3)] }),
      listed("a"),
      [],
    );
    expect(model.entries.map((e) => [e.port, e.protocol, e.connections])).toEqual([
      [53, "tcp", 0],
      [53, "udp", 0],
      [80, "tcp", 3],
    ]);
    const laid = layout(model, { networks: false, volumes: false, images: false }, 900);
    const keys = laid.edges.map((edge) => edge.key);
    expect(new Set(keys).size).toBe(keys.length);
    expect(new Set(laid.chips.map((chip) => chip.key)).size).toBe(3);
  });

  it("carries an entry's connections to its edge: its dots", () => {
    const model = mapModel(answer([node("a")], [], { entries: [entry(80, "tcp", "0.0.0.0", 3)] }), listed("a"), []);
    const host = layout(model, { networks: false, volumes: false, images: false }, 900).edges[0];
    expect(host?.connections).toBe(3);
    expect(dotRate(host as Parameters<typeof dotRate>[0])).toBeGreaterThan(0);
  });
});

describe("layout", () => {
  it("lays two nodes that call each other out one after the other", () => {
    const model = mapModel(
      answer([node("a"), node("b"), node("c")], [link("a", "b", "observed"), link("b", "a", "observed"), link("b", "c", "declared")]),
      listed("a", "b", "c"),
      [],
    );
    expect(forward(model.linked, model.edges).map((edge) => `${edge.from}>${edge.to}`)).toEqual(["b>c", "a>b"]);
    expect([...depths(model.linked, model.edges).values()]).toEqual([0, 1, 2]);
  });

  it("puts a node in the column of its longest path, each column ordered by its sources", () => {
    const model = mapModel(
      answer([node("a"), node("b"), node("c"), node("d")], [link("a", "c", "declared"), link("b", "d", "declared"), link("a", "b", "declared"), link("c", "d", "declared")]),
      listed("a", "b", "c", "d"),
      [],
    );
    expect([...depths(model.linked, model.edges)]).toEqual([
      ["a", 0],
      ["b", 1],
      ["c", 1],
      ["d", 2],
    ]);
    expect(columns(model.linked, model.edges).map((col) => col.map((n) => n.id))).toEqual([["a"], ["b", "c"], ["d"]]);
  });

  it("finds a chain's depth whatever order its links come in", () => {
    const model = mapModel(
      answer([node("a"), node("b"), node("c"), node("d")], [link("c", "d", "declared"), link("b", "c", "declared"), link("a", "b", "declared")]),
      listed("a", "b", "c", "d"),
      [],
    );
    expect([...depths(model.linked, model.edges).values()]).toEqual([0, 1, 2, 3]);
  });

  it("keeps host port chips 32 px apart, and fills the width it's given", () => {
    const model = mapModel(
      answer([node("a")], [], {
        entries: [80, 443, 8080].map((port) => ({ container: "a", host_ip: "0.0.0.0", host_port: port, container_port: port, protocol: "tcp" })),
      }),
      listed("a"),
      [],
    );
    const laid = layout(model, { networks: false, volumes: false, images: false }, 900);
    expect(laid.chips.map((chip) => chip.y)).toEqual([56, 88, 120]);
    expect(laid.width).toBe(900);
    expect(laid.edges.every((edge) => edge.host && edge.d.startsWith("M76,"))).toBe(true);
  });
});

describe("dotRate", () => {
  it("draws no dots without connections, more with more, never past its cap", () => {
    expect(dotRate({ state: "active", connections: 0 })).toBe(0);
    expect(dotRate({ state: "idle", connections: 5 })).toBe(0);
    expect(dotRate({ state: "broken", connections: 5 })).toBe(0);
    expect(dotRate({ state: "active", connections: 1 })).toBeCloseTo(0.8);
    expect(dotRate({ state: "active", connections: 3 })).toBeGreaterThan(dotRate({ state: "active", connections: 1 }));
    expect(dotRate({ state: "active", connections: 10_000 })).toBe(2.4);
  });
});
