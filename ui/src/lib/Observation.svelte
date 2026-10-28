<script lang="ts">
  import type { Observation } from './api';
  let { observation } : { observation: Observation | undefined } = $props();
  const labels = { true: 'Yes', false: 'No', unknown: 'Unknown', stale: 'Stale' };
  function status(value: string | undefined): keyof typeof labels {
    return value === 'true' || value === 'false' || value === 'stale' ? value : 'unknown';
  }
</script>
<span class="status {status(observation?.state)}">{labels[status(observation?.state)]}</span>
<span class="detail">{observation?.reason || 'No reason reported'}<br />{observation?.observed_at ? new Date(observation.observed_at).toLocaleString() : 'Not observed'}</span>
