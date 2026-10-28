<script lang="ts" module>
  // Each drum's clip path needs an ID of its own in the page.
  let drums = 0;
</script>

<script lang="ts">
  // A volume's drum (step 14.6b): the mock's `drum`
  // (docs/design/shipyard-ui.html), filled to `fill` (0–1, at least a
  // sliver), dashed when nothing mounts the volume.
  let { fill, size = 22, dashed = false }: { fill: number; size?: number; dashed?: boolean } = $props();
  const id = `dr${drums++}`;
  const height = $derived(Math.round(size * 1.2));
</script>

<svg class="drum" width={size} height={height} viewBox="0 0 22 26" aria-hidden="true"
  ><defs><clipPath {id}><path d="M1 4V22A10 3 0 0 0 21 22V4Z" /></clipPath></defs>
  <path d="M1 4V22A10 3 0 0 0 21 22V4" fill="var(--sunken)" stroke="var(--faint)" stroke-dasharray={dashed ? "2 2" : undefined} />
  <rect x="0" y={26 - Math.max(0.08, fill) * 22} width="22" height="26" fill="var(--steel)" opacity=".85" clip-path="url(#{id})" />
  <ellipse cx="11" cy="4" rx="10" ry="3" fill="var(--window)" stroke="var(--faint)" stroke-dasharray={dashed ? "2 2" : undefined} /></svg
>
