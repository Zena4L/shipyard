<script lang="ts">
  // The console (step 14.5): the mock's `termHTML` (docs/design/shipyard-ui.html),
  // element for element. Its tabs, the selection's name, image and host PID,
  // the chevron that folds it to one bar; Logs as text (a log is shown,
  // never fed to a terminal: its control sequences are gone already, the
  // client's), a time it starts with dimmed as the mock dims it, kept at
  // its end while you're there; Shell a terminal on the container
  // (`Terminal.svelte`), or why there's none.
  import type { Backend } from "./backend";
  import type { MapNode } from "./graph";
  import type { Logs } from "./store";
  import Terminal from "./Terminal.svelte";

  let {
    node,
    hostPid,
    logs,
    tab,
    collapsed,
    backend,
    ontab,
    ontoggle,
  }: {
    node: MapNode;
    /** Its command's PID on the host, while it runs. */
    hostPid: number | null;
    logs: Logs | null;
    tab: "logs" | "shell";
    collapsed: boolean;
    backend: Pick<Backend, "shellOpen" | "shellInput" | "shellResize" | "shellAck" | "shellClose">;
    ontab: (tab: "logs" | "shell") => void;
    ontoggle: () => void;
  } = $props();

  const running = $derived(node.state === "running");
  // A shell opens the first time its tab is shown on this container while
  // it runs, and is gone once it stops (never opened again unasked when
  // it starts again). Another container is another console (`{#key}` in
  // App).
  let shellOpened = $state(false);
  $effect(() => {
    if (!running) shellOpened = false;
    else if (tab === "shell") shellOpened = true;
  });
  const lines = $derived(logs?.id === node.id ? logs.lines : []);

  /** A time a line starts with (`09:01:14.208`, `2026-10-04 09:01:09 UTC`,
   * an ISO stamp), and the rest of it. */
  const STAMP = /^(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?: ?(?:UTC|Z|[+-]\d{2}:?\d{2}))?|\d{2}:\d{2}:\d{2}(?:[.,]\d+)?)(?=\s)/;
  function split(line: string): [string, string] {
    const stamp = STAMP.exec(line)?.[0] ?? "";
    return [stamp, line.slice(stamp.length)];
  }

  // Kept at its end, unless you've scrolled up to read.
  let body: HTMLDivElement | undefined = $state();
  let atEnd = true;
  $effect.pre(() => {
    void lines;
    if (body) atEnd = body.scrollHeight - body.scrollTop - body.clientHeight < 30;
  });
  $effect(() => {
    void lines;
    void tab;
    void collapsed;
    if (body && atEnd) body.scrollTop = body.scrollHeight;
  });
</script>

<div class="term">
  <div class="termtabs" role="tablist">
    <button type="button" role="tab" aria-selected={tab === "logs"} title="shipyard logs -f {node.name}" onclick={() => ontab("logs")}
      >Logs</button
    ><button type="button" role="tab" aria-selected={tab === "shell"} title="shipyard exec -it {node.name} sh" onclick={() => ontab("shell")}
      >Shell</button
    >
    <span class="dockmeta"
      ><i class="dot {node.state}"></i><b>{node.name}</b><span>{node.image}</span>{#if hostPid !== null}<span>host PID {hostPid}</span>{/if}</span
    >
    <button
      type="button"
      class="dockbtn"
      aria-expanded={!collapsed}
      aria-label="{collapsed ? 'Expand' : 'Collapse'} console"
      title="{collapsed ? 'Expand' : 'Collapse'} console"
      onclick={ontoggle}
      ><svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"
        ><path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg
      ></button
    >
  </div>
  {#if tab === "logs"}
    <div class="termbody" id="termbody" bind:this={body}
      >{#each lines as line, i (line.n)}{#if i}{"\n"}{/if}{@const [stamp, rest] = split(line.text)}{#if stamp}<span class="t">{stamp}</span
          >{/if}{rest}{/each}</div
    >
  {:else if !running}
    <div class="termbody" id="termbody"></div>
    <div class="shellline"><span>!</span> container is not running — start it to open a shell</div>
  {/if}
  <!-- Once opened, its shell lives until another container is selected or
       it stops: hidden behind Logs or a folded dock, never ended by them. -->
  {#if running && shellOpened}
    <div class="shellhost" hidden={tab !== "shell"}><Terminal id={node.id} {backend} /></div>
  {/if}
</div>
