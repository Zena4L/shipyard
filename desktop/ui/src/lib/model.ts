// What the design shows, made from what the API says (step 14.2): each row
// of the mock's Manifest, the rail's counts and host, the status bar. The
// mock's own words and formats (docs/design/shipyard-ui.html): `fmtMem`,
// its state labels, its uptime's shape. A value the API doesn't give the
// list (live CPU and memory: `/shipyard/usage`, step 14.8a2, until read)
// is `null`, shown as "—".
import type { Container } from "./backend";

/** A container's state, as the design colours it. */
export type BoxState = "running" | "paused" | "exited" | "failed" | "restarting";

/** One container, as the design shows it. */
export interface Row {
  id: string;
  /** ISO 6346: `SHPU`, six digits, a check digit. */
  owner: string;
  serial: string;
  check: string;
  name: string;
  stack: string;
  image: string;
  state: BoxState;
  exit: number | null;
  /** CPU now, percent (stats). */
  cpu: number | null;
  /** Memory now and its limit, MiB. */
  mem: number | null;
  memMax: number | null;
  ports: string;
  up: string;
}

/** The mock's state words. */
export const STATE_LABEL: Record<BoxState, string> = {
  running: "Running",
  exited: "Stopped",
  failed: "Failed",
  paused: "Paused",
  restarting: "Restarting",
};

/** The mock's memory format: MiB below a GiB, one decimal below 10. */
export function fmtMem(mib: number): string {
  if (mib >= 1024) return `${(mib / 1024).toFixed(mib % 1024 ? 1 : 0)} GiB`;
  return `${mib < 10 ? mib.toFixed(1) : Math.round(mib)} MiB`;
}

/** Bytes in a megabyte, as `fmtSize` counts them. */
export const MB = 1_000_000;

/** The mock's size format, from megabytes (`fmtSize`): GB past 1024, whole
 * MB past 10, KB below one. */
export function fmtSize(mb: number): string {
  if (mb >= 1024) return `${(mb / 1024).toFixed(2)} GB`;
  if (mb >= 10) return `${Math.round(mb)} MB`;
  if (mb >= 1) return `${mb.toFixed(1)} MB`;
  return mb > 0 ? `${Math.max(1, Math.round(mb * 1024))} KB` : "0 B";
}

/** Whether the design counts it live (its CPU shown). */
export const live = (row: Row) => row.state === "running";

/** A node of `/shipyard/links` (version 1), what the rows need of it. */
export interface LinkNode {
  id: string;
  name: string;
  serial: string;
  memory_max: number | null;
  started_at: string | null;
  project: string | null;
  service?: string | null;
  state: string;
  exit_code: number | null;
}

/** A container of Docker's list, what the rows need of it. */
export type Summary = Pick<Container, "Id" | "Image" | "Status" | "Ports">;

/** Live numbers of a container (stats), when there are. */
export interface Live {
  /** Percent of one CPU; null before a second read. */
  cpu: number | null;
  /** MiB. */
  mem: number;
  /** Its monitor's MiB (null: no cgroup of its own). */
  monitor: number | null;
}

/** `seconds` as the mock writes a span: `3h 12m`, `14m`, `2d 4h`. */
export function span(seconds: number): string {
  const m = Math.max(0, Math.floor(seconds / 60));
  const d = Math.floor(m / 1440);
  const h = Math.floor((m % 1440) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m % 60}m`;
  return `${m}m`;
}

/** Docker's "14 minutes ago" as the mock writes it: `14m ago`. */
function ago(status: string): string | null {
  const found = /(\d+|an?|About an?) (second|minute|hour|day|week|month)s? ago/.exec(status);
  if (!found) return null;
  const n = /^\d+$/.test(found[1] ?? "") ? found[1] : "1";
  const unit = { second: "s", minute: "m", hour: "h", day: "d", week: "w", month: "mo" }[found[2] as string];
  return `${n}${unit} ago`;
}

/** An ISO 6346 serial (`SHPU 304812 2`) in its three parts. */
export function codeOf(text: string): { owner: string; serial: string; check: string } {
  const [owner = "SHPU", serial = "", check = ""] = text.split(" ");
  return { owner, serial, check };
}

/** A container's state as the design colours it: an exit other than 0 is
 * a failure, a container stopping still runs. */
export function boxState(node: Pick<LinkNode, "state" | "exit_code">): BoxState {
  if (node.state === "exited" && node.exit_code !== null && node.exit_code !== 0) return "failed";
  if (node.state === "running" || node.state === "stopping") return "running";
  if (node.state === "paused" || node.state === "restarting") return node.state;
  return "exited";
}

/** The row of a container: its node (the links answer), its summary (the
 * list), its live numbers if any, `now` the time to measure from. */
export function row(node: LinkNode, summary: Summary | undefined, numbers: Live | undefined, now: number): Row {
  const { owner, serial, check } = codeOf(node.serial);
  const state = boxState(node);
  const status = summary?.Status ?? "";
  const started = node.started_at ? Date.parse(node.started_at) : NaN;
  const since = Number.isNaN(started) ? null : span((now - started) / 1000);
  const up =
    state === "running" && since
      ? since
      : state === "paused" && since
        ? `paused ${since}`
        : state === "exited" || state === "failed"
          ? `exited ${ago(status) ?? ""}`.trim()
          : state;
  const ports = (summary?.Ports ?? [])
    .filter((port) => port.PublicPort)
    .map((port) => `${port.PublicPort} → ${port.PrivatePort}/${port.Type ?? "tcp"}`);
  return {
    id: node.id,
    owner,
    serial,
    check,
    name: node.name,
    stack: node.project ?? "standalone",
    image: summary?.Image ?? "",
    state,
    exit: node.exit_code,
    cpu: numbers?.cpu ?? null,
    mem: numbers?.mem ?? null,
    memMax: node.memory_max ? node.memory_max / (1024 * 1024) : null,
    ports: ports.length ? [...new Set(ports)].join(", ") : "—",
    up,
  };
}

/** The nodes of `nodes` the list still has: a container destroyed since the
 * links were read is gone at once, not at the next load. */
export function listed<T extends { id: string }>(nodes: T[], containers: Map<string, unknown>): T[] {
  return nodes.filter((node) => containers.has(node.id));
}
