<script lang="ts">
  // The Images section (step 14.6a): the mock's `renderImages`
  // (docs/design/shipyard-ui.html), element for element. What's on disk
  // (each layer once) and what sharing saves; "Remove N unused" confirmed
  // in its line (those it lists, each by its tags: an image pulled meanwhile
  // isn't one; shared layers stay); the
  // caption saying the layer under the pointer; a row an image, its strip
  // as wide as the square root of its size against the largest; the
  // families' legend. Every string may come from an image: text.
  import { listOf } from "./actions";
  import { NO_STEP, OWN_COLOR, agoSeconds, type ImageView, type ImagesModel } from "./layers";
  import { fmtSize, type BoxState } from "./model";
  import Strata from "./Strata.svelte";

  let {
    model,
    who,
    selected,
    lit,
    now,
    busy = false,
    onselect,
    ongoto,
    onremoveall,
  }: {
    model: ImagesModel;
    /** Each container's name and state, by ID (its chips). */
    who: Map<string, { name: string; state: BoxState }>;
    selected: string | null;
    lit: string | null;
    now: number;
    /** A removal under way: Remove waits. */
    busy?: boolean;
    onselect: (id: string) => void;
    /** A container's chip: the container selected in the yard. */
    ongoto: (id: string) => void;
    /** "Remove N unused" confirmed: the images it listed. */
    onremoveall: (images: { id: string; tags: string[] }[]) => Promise<boolean>;
  } = $props();

  const CAPTION = "Hover a layer to see what it is and which images share it.";
  // What the confirm lists, taken as it opens (an image pulled since isn't
  // among them).
  let pruning: { images: ImageView[]; frees: number } | null = $state(null);
  const unused = $derived(model.unused);
  const largest = $derived(Math.max(...model.images.map((image) => image.size), 0) || 1);
  // The lit layer: one of an image's, its sharers.
  const litLayer = $derived.by(() => {
    if (lit === null) return null;
    const owners = model.sharers.get(lit) ?? [];
    const layer = owners[0]?.layers.find((l) => l.diff === lit);
    return layer ? { layer, owners } : null;
  });
  const nameOf = (id: string) => who.get(id)?.name ?? id.slice(0, 12);
  const stateOf = (id: string) => who.get(id)?.state ?? "exited";
</script>

{#if pruning}
  {@const listing = pruning}
  <div class="secsum confirming">
    <span>Remove <b>{listOf(listing.images.map((image) => image.ref))}</b>? This frees {fmtSize(listing.frees)}. Shared layers stay.</span><button
      class="btn danger"
      type="button"
      disabled={busy}
      title="shipyard image rm {listing.images.flatMap((image) => (image.tags.length ? image.tags : [image.ref])).join(' ')}"
      onclick={async () => {
        if (await onremoveall(listing.images)) pruning = null;
      }}>Remove</button
    ><button class="btn" type="button" style:margin-left="0" onclick={() => (pruning = null)}>Cancel</button>
  </div>
{:else}
  <div class="secsum">
    <span><b>{fmtSize(model.unique)}</b> on disk for {model.images.length} images. Shared layers save {fmtSize(model.naive - model.unique)}.</span
    >{#if unused.length}<button class="btn" type="button" disabled={busy} onclick={() => (pruning = { images: unused, frees: model.unusedFrees })}
        >Remove {unused.length} unused</button
      >{/if}
  </div>
{/if}
<p class="capline" id="layercap">
  {#if litLayer}<i
      class="swatch"
      style:background={litLayer.layer.color}
      style:opacity={litLayer.layer.faint ? ".55" : undefined}
      style:margin-top="3px"
    ></i><span class="capbody" title={litLayer.layer.createdBy || NO_STEP}
      ><code>{litLayer.layer.createdBy || NO_STEP}</code><span title={listOf(litLayer.owners.map((image) => image.ref))}
        >{litLayer.layer.diff.slice(0, 15)}… · {fmtSize(litLayer.layer.size)} · {#if litLayer.owners.length > 1}shared by <b
            >{listOf(litLayer.owners.map((image) => image.ref))}</b
          >, stored once{:else}only in {litLayer.owners[0]?.ref}{/if}</span
      ></span
    >{:else}{CAPTION}{/if}
</p>
<div class="rows">
  <div class="rowhead i"><span>Image</span><span>Layers, bottom → top</span><span style:text-align="right">Size</span><span>Used by</span></div>
  {#each model.images as image (image.id)}
    <div
      class="irow"
      class:sel={image.id === selected}
      tabindex="0"
      role="button"
      aria-pressed={image.id === selected}
      onclick={() => onselect(image.id)}
      onkeydown={(event) => (event.key === "Enter" || event.key === " ") && event.target === event.currentTarget && onselect(image.id)}
    >
      <div class="ref">{image.ref}<small>{image.layers.length} layer{image.layers.length === 1 ? "" : "s"} · {agoSeconds(image.created, now)}</small></div>
      <div><Strata {image} cls="bigstrata" width="{Math.max(10, Math.sqrt(image.size / largest) * 100)}%" {lit} /></div>
      <div class="num">{fmtSize(image.size)}</div>
      <div class="chips">
        {#each image.users as id (id)}<button
            class="chip"
            type="button"
            onclick={(event) => {
              event.stopPropagation();
              ongoto(id);
            }}><i class="dot {stateOf(id)}"></i>{nameOf(id)}</button
          >{:else}<span class="chip none">unused</span>{/each}
      </div>
    </div>
  {/each}
</div>
<div class="legendrow">
  {#each model.families as family, i (i)}<span><i class="swatch" style:background={family.color}></i>{family.name}</span>{/each}<span
    ><i class="swatch" style:background={OWN_COLOR} style:opacity=".55"></i>image's own layers</span
  >
</div>
