<script lang="ts">
  // A network's inspector (step 14.6b): the mock's `networkInsp`
  // (docs/design/shipyard-ui.html), element for element. Its name, subnet
  // and gateway; each container on it with its address and the host's end
  // of its veth pair; the containers that join it to another network; its
  // bridge, way out and DNS. Every string may come from a container or a
  // network: text.
  import { listOf } from "./actions";
  import type { Band } from "./storage";

  let { band, ongoto }: { band: Band; ongoto: (id: string) => void } = $props();
</script>

<div class="insp-top">
  <div class="bigcode" style:font-size="30px">
    <i class="swatch" style:background={band.color} style:width="18px" style:height="18px"></i>
    {band.name.toUpperCase()}
  </div>
  <div class="ident">
    <h3 style:font-size="16px">
      {band.full}
      {#if band.internal}<span class="pill paused">internal</span>{:else}<span class="pill">bridge</span>{/if}
    </h3>
    <p>{band.subnet ?? "—"} · gateway {band.gateway ?? "—"}</p>
  </div>
</div>
<div class="insp-scroll">
  <div class="sect">
    <h4>Endpoints <span>{band.taps.length}</span></h4>
    <div class="attlist">
      {#each band.taps as tap (tap.node.id)}<button class="attv" type="button" onclick={() => ongoto(tap.node.id)}
          ><i class="dot {tap.node.state}"></i><b>{tap.node.name}</b><em>{tap.address}{tap.veth ? ` · ${tap.veth}` : ""}</em></button
        >{:else}<p class="tnote">No containers attached.</p>{/each}
    </div>
    {#each band.bridging as { node, others } (node.id)}<p class="tnote">
        <b>{node.name}</b> is also on {listOf(others)}, so it's {band.bridging.length === 1 ? "the only path" : "a path"} between {band.name} and {listOf(others)}.
      </p>{/each}
  </div>
  <div class="sect">
    <h4>Under the hood</h4>
    <dl class="kv">
      <dt>Bridge</dt><dd>{band.bridge ?? "—"}</dd><dt>MTU</dt><dd>1500</dd>
      <dt>Egress</dt><dd>{band.internal ? "none (no masquerade rule)" : "nftables masquerade"}</dd>
      <dt>DNS</dt><dd
        >{band.dns === "running"
          ? "Shipyard's server on the gateway"
          : band.dns === "stopped"
            ? "its server isn't running"
            : band.dns === "none"
              ? "none on the default network"
              : "—"}</dd
      >
    </dl>
    {#if band.internal}<p class="tnote">
        Internal networks have no NAT rule, so containers on {band.name} can reach each other but not the internet.
      </p>{/if}
  </div>
</div>
