<script lang="ts">
  // A stand-in for the terminal in Console's tests (jsdom draws no xterm).
  let { id }: { id: string } = $props();
</script>

<div class="stub-terminal">{id}</div>
