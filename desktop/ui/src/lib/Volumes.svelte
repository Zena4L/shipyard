<script lang="ts">
  // The Volumes section (step 14.6b): the mock's `renderVolumes`
  // (docs/design/shipyard-ui.html), element for element. What the volumes
  // hold, and what no container mounts; "Delete N unused" confirmed in its
  // line (those it listed as it opened, each by name, never forced: one
  // made since isn't among them, and the engine refuses one a container
  // names meanwhile); a row a volume, its drum filled
  // against the largest, dashed when nothing mounts it. Every string may
  // come from a container or a volume: text.
  import { listOf } from "./actions";
  import Drum from "./Drum.svelte";
  import { agoIso } from "./layers";
  import { fmtSize } from "./model";
  import { fillOf, type VolumesModel } from "./storage";

  let {
    model,
    selected,
    now,
    busy = false,
    onselect,
    ongoto,
    onremoveall,
  }: {
    model: VolumesModel;
    selected: string | null;
    now: number;
    /** A deletion under way: Delete waits. */
    busy?: boolean;
    onselect: (name: string) => void;
    /** A container's chip: the container selected in the yard. */
    ongoto: (id: string) => void;
    /** "Delete N unused" confirmed: the volumes it listed. */
    onremoveall: (names: string[]) => Promise<boolean>;
  } = $props();

  // What the confirm lists, taken as it opens.
  let pruning: { names: string[]; size: string } | null = $state(null);
  const unused = $derived(model.unused);
  const unusedSize = $derived(`${model.unusedComplete ? "" : "at least "}${fmtSize(model.unusedSize)}`);
</script>

{#if pruning}
  {@const listing = pruning}
  <div class="secsum confirming">
    <span
      >Delete <b>{listOf(listing.names)}</b> and all {listing.size} of data in {listing.names.length === 1 ? "it" : "them"}? This can't be
      undone.</span
    ><button
      class="btn danger"
      type="button"
      disabled={busy}
      title="shipyard volume rm {listing.names.join(' ')}"
      onclick={async () => {
        if (await onremoveall(listing.names)) pruning = null;
      }}>Delete</button
    ><button class="btn" type="button" style:margin-left="0" onclick={() => (pruning = null)}>Cancel</button>
  </div>
{:else}
  <div class="secsum">
    <span
      ><b>{fmtSize(model.total)}</b> in {model.volumes.length} volumes.{#if unused.length}{` ${unusedSize} isn't mounted by any container.`}{/if}</span
    >{#if unused.length}<button
        class="btn"
        type="button"
        disabled={busy}
        onclick={() => (pruning = { names: unused.map((volume) => volume.name), size: unusedSize })}>Delete {unused.length} unused</button
      >{/if}
  </div>
{/if}
<div class="rows">
  <div class="rowhead v"><span></span><span>Volume</span><span>Mounted by</span><span style:text-align="right">Size</span></div>
  {#each model.volumes as volume (volume.name)}
    <div
      class="vrow"
      class:sel={volume.name === selected}
      tabindex="0"
      role="button"
      aria-pressed={volume.name === selected}
      onclick={() => onselect(volume.name)}
      onkeydown={(event) => (event.key === "Enter" || event.key === " ") && event.target === event.currentTarget && onselect(volume.name)}
    >
      <Drum fill={fillOf(volume.size, model.largest)} size={24} dashed={!volume.users.length} />
      <div class="ref">{volume.name}<small>local · {agoIso(volume.created, now)}</small></div>
      <div class="chips">
        {#each volume.users as user (user.node.id + user.path)}<button
            class="chip"
            type="button"
            onclick={(event) => {
              event.stopPropagation();
              ongoto(user.node.id);
            }}><i class="dot {user.node.state}"></i>{user.node.name} <em>{user.path}</em> {user.mode}</button
          >{:else}<span class="chip none">not mounted</span>{/each}
      </div>
      <div class="num">{volume.complete ? "" : "≥ "}{fmtSize(volume.size)}</div>
    </div>
  {/each}
</div>
<div class="legendrow">
  <span><Drum fill={0.6} size={14} /> fill shows size relative to the largest volume</span><span
    ><Drum fill={0.3} size={14} dashed /> not mounted</span
  >
</div>
