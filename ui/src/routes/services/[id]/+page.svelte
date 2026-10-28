<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { Button } from 'm3-svelte';
  import Observation from '$lib/Observation.svelte';
  import { api, errorMessage, type Service, type Metric } from '$lib/api';
  let service: Service | undefined; let metrics: Metric[] = []; let loading = true; let error = ''; let metricsError = '';
  const statuses = ['accepted', 'applied', 'running', 'functional', 'measured'] as const;
  async function load() { loading = true; error = ''; metricsError = ''; try { const id = page.params.id; if (!id) throw new Error('Service ID is missing.'); service = await api.service(id); try { metrics = (await api.metrics(id)).metrics; } catch (e) { metricsError = errorMessage(e); } } catch (e) { error = errorMessage(e); } finally { loading = false; } }
  onMount(load);
</script>
<div class="page"><a class="back" href="/">← All services</a><div class="heading"><div><h1>{service?.name || page.params.id}</h1><p class="subtitle">Published service details and observed evidence.</p></div><Button variant="outlined" onclick={load}>Refresh</Button></div>
{#if loading}<p role="status">Loading service…</p>{:else if error}<div class="error" role="alert">{error}</div>{:else if service}
<section><h2>Revision observations</h2><div class="grid">{#each statuses as status}<div class="panel"><h3>{status[0].toUpperCase() + status.slice(1)}</h3><Observation observation={service.status?.[status]} /></div>{/each}</div></section>
<section class="section"><h2>Deployments</h2>{#if !service.deployments.length}<p class="muted">No deployments returned.</p>{:else}<div class="table-wrap"><table><thead><tr><th>Name</th><th>Recipe</th><th>Artifact</th><th>Hardware</th></tr></thead><tbody>{#each service.deployments as deployment}<tr><td>{deployment.name}</td><td>{deployment.recipe}</td><td>{deployment.artifact}</td><td>{deployment.hardware}</td></tr>{/each}</tbody></table></div>{/if}</section>
<section class="section"><h2>Live performance</h2><p class="muted">Metrics are queried from the configured telemetry source; missing values are not zero.</p>{#if metricsError}<div class="error" role="alert">{metricsError}</div>{:else if !metrics.length}<p class="muted">No metrics returned.</p>{:else}<div class="table-wrap"><table><thead><tr><th>Deployment</th><th>Metric</th><th>Value</th><th>Observed</th></tr></thead><tbody>{#each metrics as metric}<tr><td>{metric.deployment}</td><td>{metric.name}</td><td>{metric.value == null ? 'Unknown' : `${metric.value} ${metric.unit}`}</td><td>{metric.observed_at ? new Date(metric.observed_at).toLocaleString() : 'Unknown'}</td></tr>{/each}</tbody></table></div>{/if}</section>{/if}</div>
