// The inspector's actions (step 14.4b): the mock's buttons, in its order
// (`renderInspector`), each with its CLI twin as its tooltip, or the reason
// it's disabled (the design's rules 5 and 7). Remove is disabled while a
// running container needs the one selected (D14.4b-2); on a running one it
// stops it first (D14.4b-1).
import type { MapModel, MapNode } from "./graph";

/** What a button asks of the core. */
export type Action = "start" | "stop" | "pause" | "resume" | "restart" | "shell" | "remove";

/** What the core can be asked (Shell is the console's). */
export type Act = Exclude<Action, "shell">;

/** A button of the actions row. */
export interface ActionButton {
  action: Action;
  label: string;
  /** Its tooltip: the CLI twin, or why it's disabled. */
  title: string;
  disabled: boolean;
}

/** The CLI's command for `action` on `node` (`force`: a remove of a
 * running one). */
export function twinOf(action: Action, node: MapNode): string {
  const verb: Record<Action, string> = {
    start: "start",
    stop: "stop",
    pause: "pause",
    resume: "unpause",
    restart: "restart",
    shell: "exec -it",
    remove: removesRunning(node) ? "rm -f" : "rm",
  };
  return `shipyard ${verb[action]} ${node.name}${action === "shell" ? " sh" : ""}`;
}

/** Whether `node` runs (paused or restarting too): a remove stops it first. */
export function removesRunning(node: MapNode): boolean {
  return node.state === "running" || node.state === "paused" || node.state === "restarting";
}

/** The containers that need `node` (an edge to it, declared or seen)
 * and run, paused or restarting too (each would lose it on its way back),
 * by name, each once. */
export function neededBy(model: MapModel, node: MapNode): string[] {
  const names = model.edges
    .filter((edge) => edge.to === node.id && edge.from !== node.id)
    .flatMap((edge) => model.all.filter((n) => n.id === edge.from && removesRunning(n)))
    .map((n) => n.name);
  return [...new Set(names)];
}

/** `names` as a sentence's list ("a, b and c"). */
export function listOf(names: string[]): string {
  return names.length === 1 ? (names[0] ?? "") : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/** The actions row for `node`: Stop or Start, Pause or Resume (running or
 * paused alone), Restart, Shell, then Remove. */
export function actionsFor(model: MapModel, node: MapNode): ActionButton[] {
  const live = removesRunning(node);
  const button = (action: Action, label: string, reason: string | null = null): ActionButton => ({
    action,
    label,
    title: reason ?? twinOf(action, node),
    disabled: reason !== null,
  });
  const buttons = [live ? button("stop", "Stop") : button("start", "Start")];
  if (node.state === "running") buttons.push(button("pause", "Pause"));
  else if (node.state === "paused") buttons.push(button("resume", "Resume"));
  // A failed one may be restarted (the CLI's restart starts it); one
  // stopped on purpose is started (the mock's rule).
  buttons.push(button("restart", "Restart", node.state === "exited" ? `${node.name} is stopped: start it` : null));
  buttons.push(button("shell", "Shell", node.state === "running" ? null : `${node.name} isn't running: start it to open a shell`));
  const needers = neededBy(model, node);
  buttons.push(button("remove", "Remove", needers.length ? `Needed by ${listOf(needers)}: stop them first` : null));
  return buttons;
}

/** The Run sheet's form as the CLI says it: detached, as the sheet runs it
 * (D14.4b-5). */
export function runTwin(form: { image: string; name: string; memory: number; port: string }): string {
  const limit = form.memory >= 1024 ? `${form.memory / 1024}g` : `${form.memory}m`;
  const named = form.name.trim() ? ` --name ${form.name.trim()}` : "";
  const published = form.port.trim() ? ` -p ${form.port.trim()}` : "";
  return `shipyard run -d${named} --memory ${limit}${published} ${form.image.trim() || "IMAGE"}`;
}
