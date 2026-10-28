<script lang="ts">
  // The mock's Manifest (renderManifest in docs/design/shipyard-ui.html),
  // element for element. Every string may come from a container: text,
  // never markup.
  import { STATE_LABEL, fmtMem, live, type Row } from "./model";

  let {
    rows,
    selected,
    onselect,
  }: { rows: Row[]; selected: string | null; onselect: (id: string) => void } = $props();

  const cpu = (row: Row) => (live(row) && row.cpu !== null ? `${row.cpu.toFixed(1)}%` : "—");
  const memory = (row: Row) =>
    `${(live(row) || row.state === "paused") && row.mem !== null ? fmtMem(row.mem) : "—"} / ${
      row.memMax !== null ? fmtMem(row.memMax) : "—"
    }`;
</script>

<div class="manifest-wrap">
  <table>
    <thead>
      <tr>
        <th>Container ID</th><th>Name</th><th>Image</th><th>State</th><th>CPU</th><th>Memory</th><th>Ports</th><th>Uptime</th>
      </tr>
    </thead>
    <tbody>
      {#each rows as row (row.id)}
        <tr
          class={row.id === selected ? "sel" : ""}
          tabindex="0"
          onclick={() => onselect(row.id)}
          onkeydown={(event) => event.key === "Enter" && onselect(row.id)}
        >
          <td><span class="stencil">{row.owner} {row.serial} {row.check}</span></td>
          <td><b>{row.name}</b></td>
          <td class="mono">{row.image}</td>
          <td><span class="pill {row.state}"><i class="dot {row.state}"></i>{STATE_LABEL[row.state]}{row.exit ? ` · ${row.exit}` : ""}</span></td>
          <td class="mono">{cpu(row)}</td>
          <td class="mono">{memory(row)}</td>
          <td class="mono">{row.ports}</td>
          <td>{row.up}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</div>
