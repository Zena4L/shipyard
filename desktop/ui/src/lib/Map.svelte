<script lang="ts">
  // The Map (step 14.3): the mock's `renderMap`
  // (docs/design/shipyard-ui.html), element for element. Containers left to
  // right by dependency depth from the ports they publish; declared links
  // solid, observed-but-undeclared dotted, refused red; dots on edges with
  // open connections; network liveries and volume chips; the status line
  // with its fix; the start order and what's connected to nothing; each
  // box's image as a strip of its layers above it (14.6a). Hover
  // traces (everything else dims), click navigates. Every string may come
  // from a container: text, never markup; positions by `style:`.
  import Box from "./Box.svelte";
  import { mapStatus, startOrders, walk, type MapModel, type MapNode, type Section } from "./graph";
  import { DOT_SPEED, HOSTW, MAX_DOTS, MAX_EDGE_DOTS, MINGAP, dotRate, layout } from "./layout";
  import type { ImagesModel } from "./layers";
  import { STATE_LABEL, fmtSize } from "./model";
  import Strata from "./Strata.svelte";

  let {
    model,
    images = null,
    lit = null,
    selected,
    onselect,
    onsection,
    onimage = () => {},
    onstart,
  }: {
    model: MapModel;
    /** The images, for the Images overlay (14.6a); none read yet: no strips. */
    images?: ImagesModel | null;
    /** The layer under the pointer, if any (lit in every strip). */
    lit?: string | null;
    selected: string | null;
    onselect: (id: string) => void;
    onsection: (section: Section, name?: string) => void;
    /** An image's strip: that image, in the Images section. */
    onimage?: (id: string) => void;
    onstart: (id: string) => void;
  } = $props();

  let overlays = $state({ networks: true, volumes: true, images: true });
  // A container's image, as the overlay draws it (by the image it runs).
  const imageOf = (node: MapNode) => images?.byContainer.get(node.id) ?? null;
  let width = $state(0);
  // What the pointer traces: `node:ID`, `net:NAME`, `vol:NAME`, `img:ID`,
  // or nothing.
  let traced: string | null = $state(null);

  const laid = $derived(layout(model, overlays, Math.max(0, width - 44)));
  const status = $derived(mapStatus(model));
  const orders = $derived(startOrders(model));
  // Why a refused node is: stopped or failed (Start's fix), running but
  // not ready (unhealthy, or a job not done: nothing to start), paused, or
  // a service with no container.
  const why = (node: MapNode) =>
    node.ghost ? "has no container" : node.state === "running" ? "isn't ready" : `is ${STATE_LABEL[node.state].toLowerCase()}`;
  const startable = (node: MapNode) => !node.ghost && (node.state === "exited" || node.state === "failed");
  const list = (names: string[]) =>
    names.length === 1 ? (names[0] ?? "") : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;

  // The trace: what stays lit, and which edges.
  const trace = $derived.by(() => {
    if (!traced) return null;
    const colon = traced.indexOf(":");
    const kind = traced.slice(0, colon);
    const name = traced.slice(colon + 1);
    if (kind !== "node") {
      const keep = new Set(
        model.linked
          .filter((node) =>
            kind === "net"
              ? node.nets.some((net) => net.full === name)
              : kind === "img"
                ? images?.byContainer.get(node.id)?.id === name
                : node.mounts.some((m) => m.vol === name),
          )
          .map((node) => node.id),
      );
      return {
        keep,
        edgeOn: (a: string, b: string) => (a === "host" || keep.has(a)) && keep.has(b),
        vol: kind === "vol" ? name : null,
      };
    }
    const down = walk(model.edges, name, "down");
    const up = walk(model.edges, name, "up");
    return {
      keep: new Set([name, ...down, ...up]),
      edgeOn: (a: string, b: string) =>
        a === "host" ? b === name || up.has(b) : ((a === name || down.has(a)) && down.has(b)) || ((b === name || up.has(b)) && up.has(a)),
      vol: null,
    };
  });
  const dim = (id: string) => trace !== null && !trace.keep.has(id);
  const dimEdge = (from: string, to: string) => trace !== null && !trace.edgeOn(from, to);

  function pointAt(target: EventTarget | null) {
    const element = target instanceof Element ? target : null;
    const box = element?.closest<HTMLElement>(".box");
    if (box) {
      // The mock's attribute, holding the node's ID.
      traced = box.dataset.name ? `node:${box.dataset.name}` : null;
      return;
    }
    traced = element?.closest<HTMLElement>("[data-hl]")?.dataset.hl ?? null;
  }

  // Each edge's label, at its path's middle: measured on the frame after a
  // layout, once the browser has laid the new paths out (measured at once,
  // a path could still be the last layout's).
  let paths: SVGPathElement[] = $state([]);
  let middles: ({ x: number; y: number } | null)[] = $state([]);
  // Each edge's length, by its key: a poll that adds or reorders edges never
  // hands a kept edge's dots another's length for a frame.
  let lengths: globalThis.Map<string, number> = $state(new globalThis.Map());
  $effect(() => {
    void laid;
    const measure = requestAnimationFrame(() => {
      const measurable = (path: SVGPathElement | undefined) => path && typeof path.getTotalLength === "function";
      const measured = paths.map((path) => (measurable(path) ? path.getTotalLength() : 0));
      lengths = new globalThis.Map(laid.edges.map((edge, i) => [edge.key, measured[i] ?? 0]));
      middles = paths.map((path, i) => {
        if (!measurable(path)) return null;
        const point = path.getPointAtLength((measured[i] ?? 0) / 2);
        return { x: point.x, y: point.y };
      });
    });
    return () => cancelAnimationFrame(measure);
  });

  // The dots: on each active edge with open connections, a few that loop
  // along it at 140 px a second, more the more connections it has; none when
  // the system asks for less motion. CSS moves them (each its edge's path as
  // its offset path), so nothing runs a frame at a time; keyed by edge, a
  // poll's new layout keeps the ones in flight.
  // Followed as it changes: asked for while the map is open, the dots stop.
  const motion = typeof matchMedia === "function" ? matchMedia("(prefers-reduced-motion: reduce)") : null;
  let reduceMotion = $state(motion?.matches ?? false);
  $effect(() => {
    const changed = () => (reduceMotion = motion?.matches ?? false);
    motion?.addEventListener("change", changed);
    return () => motion?.removeEventListener("change", changed);
  });
  const dots = $derived(
    reduceMotion
      ? []
      : laid.edges
          .flatMap((edge) => {
            const rate = dotRate(edge);
            const length = lengths.get(edge.key) ?? 0;
            if (rate <= 0 || length <= 0) return [];
            const period = length / DOT_SPEED;
            const count = Math.max(1, Math.min(MAX_EDGE_DOTS, Math.round(rate * period)));
            return Array.from({ length: count }, (_, k) => ({
              key: `${edge.key}:${k}`,
              edge,
              period,
              delay: -(k * period) / count,
            }));
          })
          .slice(0, MAX_DOTS),
  );

  const edgeClass = (edge: (typeof laid.edges)[number]) =>
    `edge ${edge.edge?.kind === "observed" ? "observed" : ""} ${edge.state === "active" ? "" : edge.state}${
      edge.from === selected || edge.to === selected ? " hot" : ""
    }`;
  const labelOf = (edge: (typeof laid.edges)[number]) => {
    const proto = edge.edge?.protocol ?? "tcp";
    if (edge.state === "broken") return edge.port === null ? "refused" : `${proto} ${edge.port} · refused`;
    if (edge.port === null) return "";
    return `${proto} ${edge.port}${edge.edge?.kind === "observed" ? " · undeclared" : ""}`;
  };
</script>

<div
  class="mapwrap"
  bind:clientWidth={width}
  role="presentation"
  onmouseover={(event) => pointAt(event.target)}
  onfocusin={(event) => pointAt(event.target)}
  onmouseleave={() => (traced = null)}
>
  {#if status.down.length}
    <div class="mapstatus warn">
      <i class="dot degraded"></i><span
        ><b>{list(status.down.map((node) => node.name))}</b>
        {status.down.length === 1 ? why(status.down[0] as MapNode) : "are down"}: {list(status.degraded.map((node) => node.name))}
        {status.degraded.length === 1 ? "is" : "are"} degraded{status.affected.length
          ? `, ${list(status.affected.map((node) => node.name))} affected through ${status.down.length === 1 ? "it" : "them"}`
          : ""}.</span
      >{#if status.down.length === 1 && startable(status.down[0] as MapNode)}<button
          class="btn"
          type="button"
          title="shipyard start {status.down[0]?.name}"
          onclick={() => status.down[0] && onstart(status.down[0].id)}>Start {status.down[0]?.name}</button
        >{/if}
    </div>
  {:else}
    <div class="mapstatus">
      <i class="dot running"></i><span
        ><b
          >{model.edges.length === 0
            ? "No links between containers yet."
            : status.active === 0
              ? "No active links."
              : status.active === 1
                ? "The one active link is healthy."
                : `All ${status.active} active links healthy.`}</b
        >
        {#if !model.observed}<span title={model.unobserved ?? ""}>Undeclared connections can't be seen here.</span
          >{:else if status.undeclared}{status.undeclared} undeclared connection{status.undeclared === 1 ? "" : "s"} detected.{:else if model
            .edges.length}Every connection is declared.{:else}A container that publishes a port, declares <code>depends_on</code> or connects to
          another is drawn here.{/if}</span
      >
    </div>
  {/if}

  <div class="maptools">
    <span>Show</span>
    <button class="toggle" type="button" aria-pressed={overlays.networks} onclick={() => (overlays.networks = !overlays.networks)}
      ><svg viewBox="0 0 12 12" aria-hidden="true"><path d="M1 9h10M3 9V4M9 9V4M6 9V2" stroke="currentColor" stroke-width="1.4" /></svg
      >Networks</button
    >
    <button class="toggle" type="button" aria-pressed={overlays.volumes} onclick={() => (overlays.volumes = !overlays.volumes)}
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
      >Volumes</button
    >
    <button class="toggle" type="button" aria-pressed={overlays.images} onclick={() => (overlays.images = !overlays.images)}
      ><svg viewBox="0 0 12 12" aria-hidden="true"><path d="M1 3.5h10M1 6h10M1 8.5h10" stroke="currentColor" stroke-width="1.6" /></svg
      >Images</button
    >
    <span class="spacer"></span>
    {#if overlays.networks}
      {#each model.networks as network (network.full)}
        <button class="netkey" type="button" data-hl="net:{network.full}" onclick={() => onsection("networks", network.full)}
          ><i class="swatch" style:background={network.color}></i>{network.name} <span>{network.subnet ?? ""}</span></button
        >
      {/each}
    {/if}
  </div>

  <div class="mapscroll">
    <div class="mapcanvas" style:width="{laid.width}px" style:height="{laid.height}px">
      <svg viewBox="0 0 {laid.width} {laid.height}" aria-hidden="true">
        <defs
          ><marker id="arr" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse"
            ><path d="M0,0.5 L7.5,4 L0,7.5 z" fill="context-stroke" /></marker
          ></defs
        >
        {#each laid.edges as edge, i (edge.key)}
          <path
            bind:this={paths[i]}
            class={edgeClass(edge)}
            class:dimmed={dimEdge(edge.from, edge.to)}
            d={edge.d}
            marker-end="url(#arr)"
          />
        {/each}
        <g>
          {#each laid.edges as edge, i (edge.key)}
            {#if !edge.host && middles[i] && labelOf(edge)}
              <text
                x={middles[i]?.x}
                y={(middles[i]?.y ?? 0) - 6}
                text-anchor="middle"
                class="elabel {edge.edge?.kind === 'observed' ? 'observed' : ''} {edge.state === 'broken' ? 'broken' : ''}"
                class:dimmed={dimEdge(edge.from, edge.to)}>{labelOf(edge)}</text
              >
            {/if}
          {/each}
        </g>
        <g>
          {#each dots as dot (dot.key)}
            <circle
              class="packet"
              r="2.4"
              class:dimmed={dimEdge(dot.edge.from, dot.edge.to)}
              style:offset-path={`path("${dot.edge.d}")`}
              style:animation-duration="{dot.period}s"
              style:animation-delay="{dot.delay}s"
            />
          {/each}
        </g>
      </svg>
      <span class="colhead" style:left="0">Host</span>
      <span class="colhead" style:left="{HOSTW + MINGAP}px">Depends on →</span>
      {#each laid.chips as chip (chip.key)}
        <span class="hostchip" style:top="{chip.y}px" class:dimmed={dimEdge("host", chip.to)}
          ><svg viewBox="0 0 12 12" aria-hidden="true"
            ><rect x="1" y="2" width="10" height="7" rx="1" fill="none" stroke="currentColor" /><path d="M4 11h4" stroke="currentColor" /></svg
          >:{chip.port}{chip.protocol === "udp" ? "/udp" : ""}</span
        >
      {/each}
      {#each model.linked as node (node.id)}
        {@const place = laid.places.get(node.id)}
        {#if place}
          <div class="mnode" class:dimmed={dim(node.id)} style:left="{place.x}px" style:top="{place.y}px" style:width="{place.w}px">
            {#if overlays.images}
              {@const image = imageOf(node)}
              {#if image && images}<button
                  type="button"
                  class="strata-row"
                  data-hl="img:{image.id}"
                  title="{image.ref} · {image.layers.length} layers"
                  onclick={() => onimage(image.id)}
                  ><Strata {image} cls="strata" {lit} /><span>{fmtSize(image.size)}</span></button
                >{:else}<span class="strata-row"></span>{/if}
            {/if}
            {#if overlays.networks}
              <span class="livery"
                >{#each node.nets as net (net.full)}<i style:background={net.color} title="{net.name} · {net.ip}"></i>{/each}</span
              >
            {/if}
            <Box {node} selected={node.id === selected} health={model.health.get(node.id) ?? null} {onselect} />
            {#if overlays.volumes}
              {#each node.mounts as mount (mount.vol + mount.path)}
                <button
                  type="button"
                  class="vchip"
                  class:lit={trace?.vol === mount.vol}
                  data-hl="vol:{mount.vol}"
                  style:--maxw="{place.maxw}px"
                  title="{mount.vol} → {mount.path} ({mount.mode})"
                  onclick={() => onsection("volumes", mount.vol)}
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
                  >{mount.vol}{#if mount.mode === "ro"}{" "}<em>ro</em>{/if}</button
                >
              {/each}
            {/if}
          </div>
        {/if}
      {/each}
    </div>
  </div>

  <div class="mapfoot">
    {#each orders as order (order.project)}
      <span class="order"
        >Start order <em>{order.project}</em>
        {#each order.tiers as tier, t (t)}{#if t}{" "}<em>→</em>{" "}{/if}{#each tier as name, n (n)}{#if n}{" · "}{/if}<b
              >{name}</b
            >{/each}{/each}</span
      >
    {/each}
    <span class="spacer"></span>
    <span
      ><svg width="26" height="8" aria-hidden="true"><line x1="1" y1="4" x2="25" y2="4" style:stroke="var(--link)" stroke-width="1.8" /></svg>
      declared</span
    >
    <span
      ><svg width="26" height="8" aria-hidden="true"
        ><line
          x1="1"
          y1="4"
          x2="25"
          y2="4"
          style:stroke="var(--warn)"
          style:stroke-dasharray="2 4"
          style:stroke-linecap="round"
          stroke-width="1.8"
        /></svg
      >
      observed</span
    >
    <span
      ><svg width="26" height="8" aria-hidden="true"
        ><line x1="1" y1="4" x2="25" y2="4" style:stroke="var(--bad)" style:stroke-dasharray="6 4" stroke-width="1.8" /></svg
      >
      refused</span
    >
    {#if model.loose.length}
      <div class="loose" style:flex-basis="100%">
        Not connected to anything {#each model.loose as node (node.id)}<button type="button" onclick={() => onselect(node.id)}
            ><i class="dot {node.state}"></i>{node.name}</button
          >{/each}
      </div>
    {/if}
  </div>
</div>
