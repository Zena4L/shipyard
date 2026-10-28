// The live numbers (step 14.8a2): the rail's memory in use, the
// Manifest's CPU and memory per container, the status bar's monitors'
// memory, from two reads of `/shipyard/usage`'s counters. A CPU share is
// Docker's CLI's: the container's CPU time over the host's between the two
// reads, times the host's CPUs (one busy CPU of four: 100 %).
import type { Usage } from "./backend";
import type { Live } from "./model";

/** A MiB, as the Manifest's memory limits are said. */
const MIB = 1024 ** 2;

/** What the window draws from a read (and the one before). */
export interface LiveNumbers {
  /** Each container's, by ID. */
  live: Record<string, Live>;
  /** The host's memory in use, bytes (null: unread). */
  memoryUsed: number | null;
}

/** The live numbers of `now`, its CPU shares against `before`. */
export function liveOf(now: Usage, before: Usage | null): LiveNumbers {
  const hostDelta = before ? now.cpu.time - before.cpu.time : 0;
  const earlier = new Map((before?.containers ?? []).map((one) => [one.id, one.cpu_time]));
  const live: LiveNumbers["live"] = {};
  for (const one of now.containers) {
    const was = earlier.get(one.id);
    const delta = was === undefined ? -1 : one.cpu_time - was;
    live[one.id] = {
      // A container new since, a host whose time didn't move, or a counter
      // that went back (started again: a new cgroup): no share yet.
      cpu: hostDelta > 0 && delta >= 0 ? (delta / hostDelta) * now.cpu.cpus * 100 : null,
      mem: one.memory / MIB,
      monitor: one.monitor_memory === null ? null : one.monitor_memory / MIB,
    };
  }
  const { total, available } = now.memory;
  return { live, memoryUsed: total > 0 && available <= total ? total - available : null };
}

/** The memory of the monitors of the containers `ids` (the rows shown), MiB:
 * null when none of them has one measured. */
export function monitorsMemory(live: Record<string, Live> | null, ids: string[]): number | null {
  const measured = ids.map((id) => live?.[id]?.monitor).filter((mib): mib is number => mib != null);
  return measured.length ? measured.reduce((sum, mib) => sum + mib, 0) : null;
}
