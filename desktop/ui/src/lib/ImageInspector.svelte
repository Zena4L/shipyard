<script lang="ts">
  // An image's inspector (step 14.6a): the mock's `imageInsp`
  // (docs/design/shipyard-ui.html), element for element. Its name and tag,
  // ID and age; Run (the Run sheet, the image in it) and Remove (disabled
  // while a container uses it, confirmed inline with what it frees and its
  // command); its layers top → bottom, each with the step that made it and
  // what shares it; who uses it. Every string may come from an image: text.
  import { listOf } from "./actions";
  import { NO_STEP, agoSeconds, type ImageView, type ImagesModel } from "./layers";
  import { fmtSize, type BoxState } from "./model";
  import Strata from "./Strata.svelte";

  let {
    image,
    model,
    who,
    lit,
    now,
    busy = false,
    onrun,
    onremove,
    ongoto,
  }: {
    image: ImageView;
    model: ImagesModel;
    who: Map<string, { name: string; state: BoxState }>;
    lit: string | null;
    now: number;
    /** A removal under way: its buttons wait. */
    busy?: boolean;
    /** Run: the Run sheet, this image in it. */
    onrun: (reference: string) => void;
    /** Remove confirmed. */
    onremove: (image: ImageView) => void;
    ongoto: (id: string) => void;
  } = $props();

  // The confirm, open (closed for another image).
  let confirming = $state(false);
  const imageId = $derived(image.id);
  $effect(() => {
    void imageId;
    confirming = false;
  });

  const shortId = $derived(image.id.replace(/^sha256:/, "").slice(0, 16));
  const split = $derived.by(() => {
    const at = image.ref.lastIndexOf(":");
    return at > image.ref.lastIndexOf("/") ? [image.ref.slice(0, at), image.ref.slice(at + 1)] : [image.ref, ""];
  });
  // By its tags, never forced (`-f` would skip the engine's in-use check):
  // every tag but the last untagged, the last checked; an untagged one by
  // its ID.
  const twin = $derived(`shipyard image rm ${image.tags.length ? image.tags.join(" ") : shortId.slice(0, 12)}`);
  // No name to run it by (an image pulled by digest alone).
  const runnable = $derived(image.tags.length > 0);
  const users = $derived(image.users.map((id) => ({ id, name: who.get(id)?.name ?? id.slice(0, 12), state: who.get(id)?.state ?? "exited" })));
  const sharersOf = (diff: string) => model.sharers.get(diff)?.length ?? 1;
</script>

<div class="insp-top">
  <div class="bigcode" style:font-size="30px">
    {(split[0]?.split("/").pop() ?? "").toUpperCase()}
    {#if split[1]}<span class="chk" style:font-size="20px" style:padding="2px 6px 1px">{split[1]}</span>{/if}
  </div>
  <div class="ident">
    <h3 style:font-size="16px">{image.ref}</h3>
    <p>sha256:{shortId}… · pulled {agoSeconds(image.created, now)}</p>
  </div>
  {#if confirming}
    <div class="confirm">
      <span>Remove <b>{image.ref}</b>? Frees {fmtSize(image.own)}. <code>{twin}</code></span><span class="spacer"></span><button
        class="btn danger"
        type="button"
        onclick={() => {
          confirming = false;
          onremove(image);
        }}>Remove</button
      ><button class="btn" type="button" onclick={() => (confirming = false)}>Keep</button>
    </div>
  {:else}
    <div class="actions">
      <button
        class="btn"
        type="button"
        disabled={!runnable}
        title={runnable ? `shipyard run -d ${image.ref}` : "It has no name to run it by"}
        onclick={() => onrun(image.ref)}>Run</button
      ><span class="spacer"
      ></span><button
        class="btn danger"
        type="button"
        disabled={users.length > 0 || busy}
        title={users.length ? `In use by ${listOf(users.map((user) => user.name))}` : twin}
        onclick={() => (confirming = true)}>Remove</button
      >
    </div>
  {/if}
</div>
<div class="insp-scroll">
  <div class="sect">
    <h4>Layers <span>{image.layers.length} · {fmtSize(image.size)}</span></h4>
    <Strata {image} cls="bigstrata" {lit} />
    <ol class="layers">
      {#each [...image.layers].reverse() as layer, i (i)}
        <li data-layer={layer.diff} class:lit={lit === layer.diff} class:dim={lit !== null && lit !== layer.diff}>
          <i class="swatch" style:background={layer.color} style:opacity={layer.faint ? ".55" : undefined}></i><code
            >{layer.createdBy || NO_STEP}</code
          ><span>{fmtSize(layer.size)}</span>{#if sharersOf(layer.diff) > 1}<em
              >shared with {sharersOf(layer.diff) - 1} other image{sharersOf(layer.diff) > 2 ? "s" : ""}</em
            >{/if}
        </li>
      {/each}
    </ol>
    <p class="tnote">
      {fmtSize(image.own)} is unique to this image. The other {fmtSize(image.size - image.own)} is shared and stored once, and each container stacks
      its own writable layer on top with overlayfs.
    </p>
  </div>
  <div class="sect">
    <h4>Used by</h4>
    {#if users.length}<div class="chips">
        {#each users as user (user.id)}<button class="chip" type="button" onclick={() => ongoto(user.id)}
            ><i class="dot {user.state}"></i>{user.name}</button
          >{/each}
      </div>{:else}<p class="tnote">No containers. Safe to remove.</p>{/if}
  </div>
</div>
