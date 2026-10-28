<script lang="ts">
  // The Networks section (step 14.6b): the mock's `renderNetworks`
  // (docs/design/shipyard-ui.html), element for element. A band a bridge
  // network: its subnet, whether it's internal, its bridge; a bus from the
  // gateway to each container on it (its address, the other network it's
  // on or the host port it publishes), and its way out. Every string may
  // come from a container or a network: text.
  import Box from "./Box.svelte";
  import type { NetworksModel } from "./storage";

  let {
    model,
    selected,
    onselect,
    ongoto,
  }: {
    model: NetworksModel;
    selected: string | null;
    /** A band: the network selected (by its full name). */
    onselect: (full: string) => void;
    /** A container's box: the container selected in the yard. */
    ongoto: (id: string) => void;
  } = $props();
</script>

{#if model.bands.length}
  <div class="secsum">
    <span
      ><b>{model.bands.length} bridge networks</b>, {model.endpoints} endpoints. Each endpoint is a veth pair: <code class="mono">eth0</code> in the
      container, the other end on the bridge.</span
    >
  </div>
{:else}
  <div class="secsum"><span>No bridge networks here: rootless containers each get their own network through pasta.</span></div>
{/if}
{#each model.bands as band (band.full)}
  <div
    class="netband"
    class:sel={band.full === selected}
    tabindex="0"
    role="button"
    aria-pressed={band.full === selected}
    style:--nc={band.color}
    onclick={() => onselect(band.full)}
    onkeydown={(event) => (event.key === "Enter" || event.key === " ") && event.target === event.currentTarget && onselect(band.full)}
  >
    <div class="nethead">
      <i class="swatch" style:background={band.color}></i><b>{band.name}</b><span class="mono">{band.subnet ?? ""}</span>{#if band.internal}<span
          class="pill paused">internal</span
        >{/if}<span class="spacer"></span><span class="mono">{band.bridge ?? ""}</span>
    </div>
    <div class="bus">
      <div class="gw"><span>gateway</span><b>{band.gateway ?? ""}</b></div>
      {#each band.taps as tap (tap.node.id)}
        <div class="tap">
          <span class="tag"
            >{#if tap.also}{@const also = tap.also}<button
                type="button"
                onclick={(event) => {
                  event.stopPropagation();
                  onselect(also.full);
                }}><i class="swatch" style:background={also.color}></i>also {also.name}</button
              >{:else if tap.hostPort && !band.internal}host :{tap.hostPort}{/if}</span
          >
          <Box node={tap.node} onselect={(id) => ongoto(id)} />
          <div class="stem"><span>{tap.address}</span></div>
        </div>
      {:else}
        <div class="tap"><span class="tag"></span><span class="tnote" style:padding="20px 0">No containers attached</span><div class="stem"></div></div>
      {/each}
      <div class="egress" class:closed={band.internal}>{band.internal ? "⊘ no route out" : "NAT → internet"}</div>
    </div>
  </div>
{/each}
