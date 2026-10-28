<script lang="ts">
  // The Yard view (step 14.4a): the mock's `renderYard` and `moveCrane`
  // (docs/design/shipyard-ui.html), element for element. A bay for each
  // compose project (and one for the standalone containers), its networks'
  // keys, a box per container; the yellow crane over the selection, moved
  // once the boxes are laid out (and again as the window is resized).
  import Box from "./Box.svelte";
  import type { MapModel, MapNode, Section } from "./graph";

  let {
    model,
    selected,
    onselect,
    onsection,
  }: {
    model: MapModel;
    selected: string | null;
    onselect: (id: string) => void;
    onsection: (section: Section, name?: string) => void;
  } = $props();

  // Keyed by project, the standalone ones by `null`: a compose project
  // named "standalone" is a bay of its own.
  const bays = $derived.by(() => {
    const projects = [...new Set(model.containers.map((node) => node.project))];
    return projects.map((project) => {
      const nodes = model.containers.filter((node) => node.project === project);
      const up = nodes.filter((node) => node.state === "running").length;
      const nets = new Map(nodes.flatMap((node) => node.nets.map((net) => [net.full, net] as const)));
      return {
        key: project ?? "",
        stack: project ?? "standalone",
        nodes,
        meta: project === null ? `${nodes.length} containers` : `compose stack · ${up}/${nodes.length} running`,
        nets: [...nets.values()],
      };
    });
  });

  // The crane: over the selected box, its cable down to it (the mock's
  // numbers: the gantry 10 px down, the trolley 18 px tall, 8 px of the
  // box's top left clear; the trolley 46 px wide, the cable 2).
  let yard: HTMLDivElement | undefined = $state();
  let crane = $state({ x: 0, cable: 0 });
  function moveCrane() {
    const box = yard?.querySelector<HTMLElement>(".box.sel");
    if (!yard || !box) {
      crane = { x: crane.x, cable: 0 };
      return;
    }
    // Where it's laid out, not where it's drawn: a hovered box is lifted
    // 2 px by a transform, and the crane mustn't follow that.
    const yr = yard.getBoundingClientRect();
    const br = box.getBoundingClientRect();
    const drawn = getComputedStyle(box).transform;
    const lift = new DOMMatrixReadOnly(drawn === "none" ? undefined : drawn);
    crane = {
      x: br.left - yr.left - lift.m41 + Math.min(br.width / 2, 60),
      cable: Math.max(0, br.top - yr.top - lift.m42 - 10 - 18 - 8),
    };
  }
  $effect(() => {
    void selected;
    void bays;
    const frame = requestAnimationFrame(moveCrane);
    return () => cancelAnimationFrame(frame);
  });
  $effect(() => {
    if (!yard || typeof ResizeObserver !== "function") return;
    const watch = new ResizeObserver(moveCrane);
    watch.observe(yard);
    return () => watch.disconnect();
  });
  const healthOf = (node: MapNode) => model.health.get(node.id) ?? null;
</script>

<div class="yard" bind:this={yard}>
  <div class="gantry" aria-hidden="true">
    <div class="rail-beam"></div>
    <div class="trolley" style:transform="translateX({crane.x - 23}px)"></div>
    <div class="cable" style:transform="translateX({crane.x - 1}px)" style:height="{crane.cable}px"></div>
  </div>
  <div style:display="grid" style:gap="26px">
    {#each bays as bay (bay.key)}
      <section class="bay">
        <div class="bayhead">
          <h2>{bay.stack}</h2>
          <p>{bay.meta}</p>
          <span class="spacer"></span>{#each bay.nets as net (net.full)}<button
              class="netkey"
              type="button"
              data-hl="net:{net.full}"
              onclick={() => onsection("networks", net.full)}><i class="swatch" style:background={net.color}></i>{net.name}</button
            >{/each}
        </div>
        <div class="berths">
          {#each bay.nodes as node (node.id)}<Box {node} selected={node.id === selected} health={healthOf(node)} {onselect} />{/each}
        </div>
      </section>
    {/each}
  </div>
  <div class="legend">
    <span><i style:width="16px"></i>20′ — memory.max ≤ 512 MiB</span>
    <span><i style:width="32px"></i>40′ — memory.max &gt; 512 MiB</span>
    <span><i style:width="16px" style:background="transparent" style:border="1.5px dashed var(--faint)"></i>stopped</span>
    <span><i style:width="16px" style:box-shadow="inset 0 -3px 0 var(--bad)"></i>failed</span>
  </div>
</div>
