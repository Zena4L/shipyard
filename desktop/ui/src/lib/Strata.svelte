<script lang="ts">
  // One image's layers as a strip, bottom → top (the mock's `strataHTML`):
  // widths by size (a layer at least 3.5 % of the image's), coloured by its
  // family, its own layers faint. A layer lit (the one under the pointer,
  // wherever it is) and the rest dimmed by App's `lit`, as the mock's
  // `litLayer` does: each carries its digest (`data-layer`).
  import type { ImageView } from "./layers";

  let {
    image,
    cls,
    width = undefined,
    lit,
  }: {
    image: ImageView;
    /** `strata`, `strata mini` or `bigstrata`. */
    cls: string;
    width?: string | undefined;
    /** The digest of the layer under the pointer, if any. */
    lit: string | null;
  } = $props();

  const total = $derived(image.size || 1);
</script>

<span class={cls} style:width
  >{#each image.layers as layer, i (i)}<i
      data-layer={layer.diff}
      class:lit={lit === layer.diff}
      class:dim={lit !== null && lit !== layer.diff}
      style:flex="{Math.max(layer.size, total * 0.035)} 1 0"
      style:background={layer.color}
      style:opacity={layer.faint ? ".55" : undefined}
    ></i>{/each}</span
>
