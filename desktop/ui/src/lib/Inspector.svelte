<script lang="ts">
  // The inspector of a container (step 14.4a): the mock's `renderInspector`,
  // `depsHTML`, `attachedHTML` and `metricsHTML`
  // (docs/design/shipyard-ui.html), element for element. Its ISO code, name,
  // state and image; an out-of-memory kill said; what it needs and what
  // needs it, what a stop would do, and the lines to add for a connection
  // compose doesn't declare (D14-4: to copy, never written to the user's
  // file); what's attached; its live CPU and memory against `memory.max`;
  // its isolation as the kernel has it; its ports. Its actions (14.4b), each
  // with its CLI twin as its tooltip, Remove confirmed inline (`actions.ts`).
  // Every string may come from a container: text.
  import { NAMESPACE_KINDS, type Container } from "./backend";
  import { actionsFor, listOf, removesRunning, twinOf, type Act, type Action } from "./actions";
  import { dependsOnYaml, hintsFor, hostNeeds, impact, subtree, type Branch } from "./deps";
  import type { MapModel, MapNode, Section } from "./graph";
  import { STATE_LABEL, fmtMem, fmtSize, type Row } from "./model";
  import { SAMPLES, type Focus } from "./store";
  import type { ImagesModel } from "./layers";
  import Strata from "./Strata.svelte";

  let {
    node,
    row,
    model,
    summary,
    focus,
    rootless,
    images = null,
    lit = null,
    onimage = () => {},
    busy = false,
    onact,
    onshell,
    onselect,
    onsection,
  }: {
    node: MapNode;
    row: Row;
    model: MapModel;
    summary: Container | undefined;
    focus: Focus | null;
    rootless: boolean;
    /** The images (14.6a): its image's strip in its family's colours. */
    images?: ImagesModel | null;
    /** The layer under the pointer, if any. */
    lit?: string | null;
    /** Its image: that image, in the Images section. */
    onimage?: (id: string) => void;
    /** An action is under way on it: its buttons wait. */
    busy?: boolean;
    /** A button's action (`force`: a remove that stops it first). */
    onact: (action: Act, force: boolean) => void;
    /** Shell: the console's Shell tab opened. */
    onshell: () => void;
    onselect: (id: string) => void;
    onsection: (section: Section, name?: string) => void;
  } = $props();

  const health = $derived(model.health.get(node.id) ?? null);
  const dotOf = (n: MapNode) => (model.health.get(n.id) === "degraded" ? "degraded" : n.state);
  const down = $derived(subtree(model, node.id, "down"));
  const up = $derived(subtree(model, node.id, "up"));
  const host = $derived(hostNeeds(model, node.id));
  const said = $derived(impact(model, node));
  const hints = $derived(hintsFor(model, node));
  const outs = $derived(model.edges.filter((edge) => edge.from === node.id).length);
  const ins = $derived(model.edges.filter((edge) => edge.to === node.id).length);
  // A hint opened to its lines, by its target, and what its Copy did;
  // closed for another container.
  let opened: string | null = $state(null);
  let copied: string | null = $state(null);
  // Remove's inline confirm, open (closed for another container).
  let confirming = $state(false);
  // By its ID alone: the node is rebuilt at every poll of the links and
  // every event, and what's open must stay open across them.
  const nodeId = $derived(node.id);
  $effect(() => {
    void nodeId;
    opened = null;
    copied = null;
    confirming = false;
  });
  const buttons = $derived(actionsFor(model, node));
  // A confirm opened before a running container came to need it closes:
  // the confirm never removes what Remove itself wouldn't (D14.4b-2).
  const removable = $derived(buttons.some((button) => button.action === "remove" && !button.disabled));
  $effect(() => {
    if (!removable) confirming = false;
  });
  // Remove asks first; Shell opens the console's.
  function press(action: Action) {
    if (action === "remove") confirming = true;
    else if (action === "shell") onshell();
    else onact(action, false);
  }
  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      copied = "Copied";
    } catch {
      // A web view without the clipboard: the lines can still be selected.
      copied = "Select the lines to copy them";
    }
  }

  const binds = $derived(
    (summary?.Mounts ?? []).filter((mount) => mount.Type === "bind").map((mount) => ({
      src: mount.Source ?? "",
      path: mount.Destination ?? "",
      mode: mount.RW === false ? "ro" : "rw",
    })),
  );
  const running = $derived(node.state === "running");
  // Its image, as the Images section draws it (by the image it runs).
  const image = $derived(images?.byContainer.get(node.id) ?? null);
  const oom = $derived(focus?.inspected?.State?.OOMKilled === true);

  // Resources: the last samples, at least `SAMPLES` points (the oldest as 0).
  const MIB = 1024 * 1024;
  const series = $derived.by(() => {
    const cpu = (focus?.samples ?? []).map((sample) => sample.cpu);
    return [...Array(Math.max(0, SAMPLES - cpu.length)).fill(0), ...cpu];
  });
  const last = $derived(focus?.samples.at(-1) ?? null);
  // No limit of its own: the host's, as its stats say it.
  const memMax = $derived(row.memMax ?? (last ? last.limit / MIB : null));
  const mem = $derived(last ? last.mem / MIB : null);
  const pct = $derived(mem !== null && memMax ? Math.min(100, (mem / memMax) * 100) : 0);
  const spark = $derived.by(() => {
    const W = 320;
    const H = 44;
    const n = series.length;
    const top = Math.max(10, ...series) || 1;
    const pts = series.map((v, i) => [(i / (n - 1)) * W, H - 3 - (v / top) * (H - 8)] as const);
    const line = pts.map((p) => p.map((v) => v.toFixed(1)).join(",")).join(" ");
    return { W, H, line, last: pts[n - 1] ?? [0, 0] };
  });

  const isolation = $derived(focus?.isolation ?? null);
  const own = $derived(new Set((isolation?.namespaces ?? []).filter((ns) => ns.own).map((ns) => ns.kind)));
</script>

<!-- A tree's branches; `under`, in a Needed by tree, the node whose
     published ports are the host's needs at this level. -->
{#snippet tree(branches: Branch[], under: MapNode | null)}
  {@const needs = under ? hostNeeds(model, under.id) : []}
  {#if branches.length || needs.length}
    <ul>
      {#each branches as branch (branch.node.id)}
        <li>
          <span class="tnode"
            ><button class="tlink" type="button" onclick={() => onselect(branch.node.id)}
              ><i class="dot {dotOf(branch.node)}"></i>{branch.node.name}</button
            ><span class="tmeta {branch.kind === 'declared' ? '' : branch.kind}">{branch.meta}</span></span
          >{@render tree(branch.children, under ? branch.node : null)}
        </li>
      {/each}
      {#each needs as need (need.port)}
        <li>
          <span class="tnode"
            ><span class="troot" style:font-weight="500" style:color="var(--muted)">host</span><span class="tmeta"
              >:{need.port} → {under?.name}</span
            ></span
          >
        </li>
      {/each}
    </ul>
  {/if}
{/snippet}

<div class="insp-top">
  <div class="bigcode" aria-label="Container ID {node.owner} {node.serial} {node.check}">
    {node.owner}
    {node.serial} <span class="chk">{node.check}</span>
  </div>
  <div class="ident">
    <h3>
      {node.name} <span class="pill {node.state}"><i class="dot {node.state}"></i>{STATE_LABEL[node.state]}</span
      >{#if health === "degraded"}<span class="pill degraded"><i class="dot degraded"></i>Degraded</span>{/if}
    </h3>
    <p>{node.image}</p>
  </div>
  {#if oom}
    <div class="alert">
      <b>Killed: out of memory (exit {row.exit ?? 137})</b>{node.name} hit its {row.memMax !== null ? fmtMem(row.memMax) : ""}
      <code>memory.max</code> limit. Raise the limit or reduce what it holds, then restart.
    </div>
  {/if}
  {#if confirming}
    <div class="confirm">
      <span
        >Remove <b>{node.name}</b>? {removesRunning(node)
          ? "It's stopped, and its writable layer deleted."
          : "Its writable layer is deleted."} <code>{twinOf("remove", node)}</code></span
      ><span class="spacer"></span><button
        class="btn danger"
        type="button"
        disabled={busy}
        onclick={() => {
          confirming = false;
          onact("remove", removesRunning(node));
        }}>Remove</button
      ><button class="btn" type="button" onclick={() => (confirming = false)}>Keep</button>
    </div>
  {:else}
    <div class="actions">
      {#each buttons as button (button.action)}{#if button.action === "remove"}<span class="spacer"></span>{/if}<button
          class="btn"
          class:danger={button.action === "remove"}
          type="button"
          title={button.title}
          disabled={button.disabled || busy}
          onclick={() => press(button.action)}
          >{button.label}</button
        >{/each}
    </div>
  {/if}
</div>
<div class="insp-scroll">
  {#if !down.length && !up.length && !host.length}
    <div class="sect">
      <h4>Dependencies</h4>
      <p class="tnote">{node.name} doesn't depend on anything, and nothing depends on it.</p>
    </div>
  {:else}
    <div class="sect">
      <h4>Dependencies <span>{outs} out · {ins} in</span></h4>
      {#if down.length}
        <div class="treegrp">
          <span>Needs</span>
          <ul class="tree"><li><span class="troot"><i class="dot {dotOf(node)}"></i>{node.name}</span>{@render tree(down, null)}</li></ul>
        </div>
      {/if}
      {#if up.length || host.length}
        <div class="treegrp">
          <span>Needed by</span>
          <ul class="tree"><li><span class="troot"><i class="dot {dotOf(node)}"></i>{node.name}</span>{@render tree(up, node)}</li></ul>
        </div>
      {/if}
      {#if said?.kind === "down"}
        <div class="warnbox">
          <b>{listOf(said.direct)}</b>
          {said.direct.length === 1 ? "is" : "are"} degraded because {node.name} is {STATE_LABEL[node.state].toLowerCase()}. Start it to restore {said
            .direct.length === 1
            ? "it"
            : "them"}.
        </div>
      {:else if said?.kind === "if-stops"}
        <p class="tnote">
          If {node.name} stops, <b>{listOf(said.direct)}</b>
          lose{said.direct.length === 1 ? "s" : ""} a dependency{#if said.indirect.length}, and <b>{listOf(said.indirect)}</b>
            {said.indirect.length === 1 ? "is" : "are"} affected through {said.direct.length === 1 ? "it" : "them"}{/if}.
        </p>
      {:else if said?.kind === "contained"}
        <p class="tnote">Nothing depends on {node.name}{said.failed ? ", so the failure is contained" : ""}.</p>
      {/if}
      {#each hints as hint (hint.to.id)}
        <div class="hintbox">
          <span
            >{node.name} connects to <b>{hint.to.name}{hint.port !== null ? `:${hint.port}` : ""}</b> but doesn't declare it, so it can
            start before {hint.to.name} is ready.</span
          >
          {#if hint.service && opened === hint.to.id}
            {@const yaml = dependsOnYaml(hint.service, hint.target)}
            <pre class="files">{yaml}</pre>
            {#if node.composeFile}<span class="tnote">Merge into {node.composeFile}, under its own services:</span>{/if}
            <button class="btn" type="button" onclick={() => copy(yaml)}>Copy</button>{#if copied}<span class="tnote">{copied}</span>{/if}
          {:else if hint.service}
            <button class="btn" type="button" onclick={() => (opened = hint.to.id)}>Add {hint.to.name} to depends_on</button>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
  <div class="sect">
    <h4>Attached</h4>
    <div class="att">
      <span class="attk">Image</span>
      <div class="attlist">
        {#if image && images}<button class="attv" type="button" onclick={() => onimage(image.id)}
            ><Strata {image} cls="strata mini" {lit} /><b>{node.image}</b><em
              >{image.layers.length} layers · {fmtSize(image.size)}</em
            ></button
          >{:else}<span class="attv"><b>{node.image}</b></span>{/if}
      </div>
      <span class="attk">Networks</span>
      <div class="attlist">
        {#each node.nets as net (net.full)}<button class="attv" type="button" data-hl="net:{net.full}" onclick={() => onsection("networks", net.full)}
            ><i class="swatch" style:background={net.color}></i><b>{net.name}</b><em>{net.ip}</em></button
          >{:else}<span class="attv"><em>none</em></span>{/each}
      </div>
      <span class="attk">Storage</span>
      <div class="attlist">
        {#each node.mounts as mount (mount.vol + mount.path)}<button class="attv" type="button" onclick={() => onsection("volumes", mount.vol)}
            ><svg class="ico" width="11" height="12" viewBox="0 0 22 26" aria-hidden="true"
              ><path d="M1 4V22A10 3 0 0 0 21 22V4" fill="none" stroke="currentColor" stroke-width="2.4" /><ellipse
                cx="11"
                cy="4"
                rx="10"
                ry="3"
                fill="none"
                stroke="currentColor"
                stroke-width="2.4"
              /></svg
            ><b>{mount.vol}</b><em>{mount.path} · {mount.mode}</em></button
          >{/each}{#each binds as bind (bind.src + bind.path)}<span class="attv"
            ><svg class="ico" width="12" height="11" viewBox="0 0 12 11" aria-hidden="true"
              ><path d="M1 2h3.5l1 1.3H11V10H1z" fill="none" stroke="currentColor" /></svg
            ><b>{bind.src}</b><em>{bind.path} · {bind.mode} · bind</em></span
          >{/each}{#if !node.mounts.length && !binds.length}<p class="tnote" style:padding="3px 6px">
            None. Writes go to the container's own layer and are lost when it's removed.
          </p>{/if}
      </div>
    </div>
  </div>
  <div class="sect">
    <h4>Resources <span title={focus?.ended ?? undefined}>{running ? (focus?.ended ? "not live" : "live · 1s") : ""}</span></h4>
    <div id="metrics" style:display="grid" style:gap="12px">
      <div class="metric">
        <span class="lbl">CPU</span><span class="val">{running && last ? `${last.cpu.toFixed(1)}%` : "—"}</span><svg
          class="spark"
          viewBox="0 0 {spark.W} {spark.H}"
          preserveAspectRatio="none"
          aria-hidden="true"
        >
          <line x1="0" x2={spark.W} y1={spark.H / 2} y2={spark.H / 2} stroke="var(--line)" stroke-dasharray="2 4" />
          <polygon points="0,{spark.H} {spark.line} {spark.W},{spark.H}" fill="var(--steel)" fill-opacity=".14" />
          <polyline points={spark.line} fill="none" stroke="var(--steel)" stroke-width="1.6" vector-effect="non-scaling-stroke" />
          <circle cx={spark.last[0]} cy={spark.last[1]} r="3" fill="var(--accent)" stroke="var(--window)" stroke-width="1.5" />
        </svg>
      </div>
      <div class="metric" style:padding-bottom="12px">
        <span class="lbl">Memory</span><span class="val"
          >{node.state === "exited" || mem === null ? "—" : fmtMem(mem)} / {memMax !== null ? fmtMem(memMax) : "—"}</span
        >
        <div class="membar" class:full={pct >= 99}><i style:width="{node.state === 'exited' ? 0 : pct}%"></i><b></b></div>
      </div>
    </div>
  </div>
  <div class="sect">
    {#if isolation}
      <h4>Isolation <span>{own.size}/{NAMESPACE_KINDS.length} namespaces</span></h4>
      <div class="ns">{#each NAMESPACE_KINDS as kind (kind)}<span class:on={own.has(kind)}>{kind}</span>{/each}</div>
      <dl class="kv">
        <dt>cgroup</dt>
        <dd>{isolation.cgroup ?? "—"}</dd>
        <dt>PID 1</dt>
        <dd>{isolation.command ? isolation.command.args.join(" ") : "—"}</dd>
        <dt>Host PID</dt>
        <dd>{isolation.command?.pid ?? "—"}</dd>
        <dt>User</dt>
        <dd>
          {isolation.user ? `root → uid ${isolation.user.outside}${rootless ? " (rootless)" : ""}` : "the host's users"}
        </dd>
      </dl>
    {:else}
      <h4>Isolation</h4>
      <p class="tnote">Its namespaces, cgroup and processes are the kernel's to say while it runs.</p>
    {/if}
  </div>
  <div class="sect">
    <h4>Ports</h4>
    <dl class="kv"><dt>Published</dt><dd>{row.ports}</dd><dt>Uptime</dt><dd>{row.up}</dd></dl>
  </div>
</div>
