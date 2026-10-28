<script lang="ts">
  // A container drawn as a shipping container: the mock's `boxHTML`
  // (docs/design/shipyard-ui.html), element for element. Its code is the
  // ISO 6346 serial, its lamp its state, its length its memory limit (40′
  // over 512 MiB), its outline its health. Every string may come from a
  // container: text, never markup.
  import type { MapNode } from "./graph";
  import { STATE_LABEL } from "./model";

  let {
    node,
    selected = false,
    health = null,
    onselect,
  }: {
    node: MapNode;
    selected?: boolean;
    health?: "degraded" | "affected" | null;
    onselect: (id: string) => void;
  } = $props();
</script>

<button
  type="button"
  class="box {node.state}"
  class:ft40={node.big}
  class:degraded={health === "degraded"}
  class:affected={health === "affected"}
  class:sel={selected}
  data-name={node.id /* the mock's attribute; here the node's ID */}
  aria-pressed={selected}
  aria-label="{node.name}, {STATE_LABEL[node.state]}"
  onclick={() => onselect(node.id)}
>
  <span class="code">{node.owner} {node.serial} {node.check}</span>
  <span class="lamp"></span>
  <span><span class="nm">{node.name}</span><span class="sub">{node.image}</span></span>
</button>
