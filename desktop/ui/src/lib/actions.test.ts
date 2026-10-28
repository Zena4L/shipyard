import { describe, expect, it } from "vitest";
import { actionsFor, neededBy, runTwin, twinOf } from "./actions";
import type { Container, LinkEdge, Links } from "./backend";
import { mapModel, type MapModel } from "./graph";

const link = (from: string, to: string, kind: LinkEdge["kind"]): LinkEdge => ({
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
});

/** api → db (declared), worker → db (seen); `states` overrides each one's
 * (running by default), `codes` its exit. */
function model(states: Record<string, string> = {}, codes: Record<string, number> = {}): MapModel {
  const ids = ["api", "db", "worker"];
  const node = (id: string) => ({
    id,
    name: id,
    serial: "SHPU 000001 0",
    memory_max: null,
    started_at: null,
    project: "shop",
    service: id,
    state: states[id] ?? "running",
    exit_code: codes[id] ?? null,
  });
  const links: Links = {
    version: 1,
    containers: ids.map(node),
    links: [link("api", "db", "declared"), link("worker", "db", "observed")],
    observed: { available: true, cause: null, reason: null },
  };
  const containers = new Map<string, Container>(ids.map((id) => [id, { Id: id, State: states[id] ?? "running" }]));
  return mapModel(links, containers, []);
}
const nodeOf = (m: MapModel, id: string) => m.all.find((n) => n.id === id)!;
const row = (m: MapModel, id: string) =>
  actionsFor(m, nodeOf(m, id)).map((b) => [b.label, b.disabled ? `(${b.title})` : b.title]);

describe("the actions", () => {
  it("offers the mock's buttons for each state, each with its command", () => {
    const m = model({ worker: "paused" });
    expect(row(m, "api")).toEqual([
      ["Stop", "shipyard stop api"],
      ["Pause", "shipyard pause api"],
      ["Restart", "shipyard restart api"],
      ["Shell", "shipyard exec -it api sh"],
      ["Remove", "shipyard rm -f api"],
    ]);
    expect(row(m, "worker").map(([label]) => label)).toEqual(["Stop", "Resume", "Restart", "Shell", "Remove"]);
    expect(row(m, "worker")[1]).toEqual(["Resume", "shipyard unpause worker"]);
    // A failed one may be restarted (a stopped one is started).
    const failed = model({ api: "exited" }, { api: 137 });
    expect(row(failed, "api")[1]).toEqual(["Restart", "shipyard restart api"]);
    const stopped = model({ api: "exited" });
    expect(row(stopped, "api")).toEqual([
      ["Start", "shipyard start api"],
      ["Restart", "(api is stopped: start it)"],
      ["Shell", "(api isn't running: start it to open a shell)"],
      ["Remove", "shipyard rm api"],
    ]);
  });

  it("keeps Remove from a container that running ones need, naming them", () => {
    const m = model();
    expect(neededBy(m, nodeOf(m, "db"))).toEqual(["api", "worker"]);
    expect(row(m, "db").at(-1)).toEqual(["Remove", "(Needed by api and worker: stop them first)"]);
    // A paused one holds it too (it would lose it on resuming).
    const paused = model({ api: "paused", worker: "exited" });
    expect(neededBy(paused, nodeOf(paused, "db"))).toEqual(["api"]);
    // Its stopped dependents don't hold it.
    const quiet = model({ api: "exited", worker: "exited" });
    expect(neededBy(quiet, nodeOf(quiet, "db"))).toEqual([]);
    expect(row(quiet, "db").at(-1)).toEqual(["Remove", "shipyard rm -f db"]);
    expect(twinOf("remove", nodeOf(quiet, "api"))).toBe("shipyard rm api");
  });

  it("says the Run sheet's form as the CLI would, detached", () => {
    expect(runTwin({ image: " alpine:3.20 ", name: "", memory: 256, port: "" })).toBe("shipyard run -d --memory 256m alpine:3.20");
    expect(runTwin({ image: "nginx", name: "web", memory: 2048, port: "8081:80" })).toBe(
      "shipyard run -d --name web --memory 2g -p 8081:80 nginx",
    );
    expect(runTwin({ image: "", name: "", memory: 1024, port: "" })).toBe("shipyard run -d --memory 1g IMAGE");
  });
});
