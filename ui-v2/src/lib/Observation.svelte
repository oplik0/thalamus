<script lang="ts">
  import type { Observation } from './api';
  let { observation } : { observation: Observation | undefined } = $props();
  const labels = { true: 'Yes', false: 'No', unknown: 'Unknown', stale: 'Stale' };
</script>
<span class="status {observation?.state ?? 'unknown'}">{labels[observation?.state ?? 'unknown']}</span>
<span class="detail">{observation?.reason || 'No reason reported'}<br />{observation?.observed_at ? new Date(observation.observed_at).toLocaleString() : 'Not observed'}</span>
