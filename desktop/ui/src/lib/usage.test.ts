import { describe, expect, it } from "vitest";
import type { Usage } from "./backend";
import { liveOf, monitorsMemory } from "./usage";

const MIB = 1024 ** 2;

/** A read: the host's CPU time `host` on `cpus` CPUs, each container's
 * `[id, cpu time, memory, its monitor's memory]`. */
const read = (host: number, cpus: number, containers: [string, number, number, (number | null)?][]): Usage => ({
  memory: { total: 4096 * MIB, available: 3072 * MIB },
  cpu: { time: host, cpus },
  containers: containers.map(([id, cpu_time, memory, monitor_memory = null]) => ({ id, cpu_time, memory, monitor_memory })),
});

describe("liveOf", () => {
  it("says a container's CPU as Docker's CLI does: its share of the host's time, times its CPUs", () => {
    const before = read(10_000, 4, [["a", 1_000, 0]]);
    // A quarter of the host's time on four CPUs: one CPU busy, 100 %.
    const now = read(18_000, 4, [["a", 3_000, 64 * MIB]]);
    const numbers = liveOf(now, before);
    expect(numbers.live.a).toEqual({ cpu: 100, mem: 64, monitor: null });
  });

  it("has no CPU share on a first read, for a container new since, or one started again", () => {
    expect(liveOf(read(10, 2, [["a", 5, 0]]), null).live.a?.cpu).toBeNull();
    const before = read(10, 2, [["a", 500, 0]]);
    const now = read(20, 2, [
      ["a", 5, 0], // its counter went back: a new cgroup
      ["b", 5, 0], // not in the read before
    ]);
    expect(liveOf(now, before).live.a?.cpu).toBeNull();
    expect(liveOf(now, before).live.b?.cpu).toBeNull();
    // The host's time didn't move: nothing to divide by.
    expect(liveOf(read(10, 2, [["a", 600, 0]]), before).live.a?.cpu).toBeNull();
  });

  it("says the memory in use as the host's total less what it has available", () => {
    expect(liveOf(read(0, 1, []), null).memoryUsed).toBe(1024 * MIB);
    // Unread: no number.
    expect(liveOf({ ...read(0, 1, []), memory: { total: 0, available: 0 } }, null).memoryUsed).toBeNull();
  });

  it("says the monitors' memory of the containers shown, each its own", () => {
    const { live } = liveOf(read(0, 1, [["a", 0, 0, 2 * MIB], ["b", 0, 0, MIB / 2], ["c", 0, 0, null]]), null);
    expect(live.a?.monitor).toBe(2);
    // Only the shown containers' (a container of another store, or not
    // shown, isn't counted), and one with no cgroup of its own says nothing.
    expect(monitorsMemory(live, ["a", "c"])).toBe(2);
    expect(monitorsMemory(live, ["a", "b"])).toBe(2.5);
    expect(monitorsMemory(live, ["c"])).toBeNull();
    expect(monitorsMemory(null, ["a"])).toBeNull();
  });
});
