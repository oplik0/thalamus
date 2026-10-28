<script lang="ts">
  import { onMount } from 'svelte';
  import { Button } from 'm3-svelte';
  import Observation from '$lib/Observation.svelte';
  import { api, errorMessage, type Service } from '$lib/api';
  let services: Service[] = []; let loading = true; let error = '';
  const statuses = ['accepted', 'applied', 'running', 'functional', 'measured'] as const;
  async function load() { loading = true; error = ''; try { services = (await api.services()).services; } catch (e) { error = errorMessage(e); } finally { loading = false; } }
  onMount(load);
</script>
<div class="page">
  <div class="heading"><div><h1>Published services</h1><p class="subtitle">The models your teams can call, from desired state through live evidence.</p></div><Button variant="outlined" onclick={load}>Refresh</Button></div>
  {#if loading}<p role="status">Loading services…</p>{:else if error}<div class="error" role="alert">{error}</div>{:else if !services.length}<div class="panel"><p>No published services were returned.</p></div>{:else}
  <div class="table-wrap"><table><thead><tr><th>Service</th>{#each statuses as status}<th>{status}</th>{/each}</tr></thead><tbody>{#each services as service}<tr><td><a href="/services/{encodeURIComponent(service.name)}">{service.name}</a><span class="detail">{service.description || `${service.deployments.length} deployments`}</span></td>{#each statuses as status}<td><Observation observation={service.status?.[status]} /></td>{/each}</tr>{/each}</tbody></table></div>
  <p class="muted">Each observation is independent and includes its source reason and observation time. Unknown and stale are not healthy states.</p>{/if}
</div>
