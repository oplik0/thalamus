<script lang="ts">
  import { onMount } from 'svelte'; import { Button } from 'm3-svelte';
  import { api, errorMessage, type Project } from '$lib/api';
  let projects: Project[] = []; let loading = true; let loaded = false; let busy = false; let error = ''; let name = ''; let team = ''; let budget: number | undefined;
  async function load() { loading = true; loaded = false; error = ''; try { projects = (await api.projects()).projects; loaded = true; } catch (e) { error = errorMessage(e); } finally { loading = false; } }
  async function create() { busy = true; error = ''; try { await api.createProject({ name, team, budget: budget ?? null }); name = ''; team = ''; budget = undefined; loaded = false; projects = (await api.projects()).projects; loaded = true; } catch (e) { error = errorMessage(e); } finally { busy = false; } }
  onMount(load);
</script>
<div class="page"><div class="heading"><div><h1>Projects</h1><p class="subtitle">Group keys and attribute usage to the teams they serve.</p></div><Button variant="outlined" onclick={load}>Refresh</Button></div>
{#if error}<div class="error" role="alert">{error}</div>{/if}{#if loading}<p role="status">Loading projects…</p>{:else if loaded && !projects.length}<p class="muted">No projects returned.</p>{:else if loaded}<div class="table-wrap"><table><thead><tr><th>Name</th><th>Team</th><th>Budget</th></tr></thead><tbody>{#each projects as project}<tr><td>{project.name}</td><td>{project.team}</td><td>{project.budget ?? 'Not set'}</td></tr>{/each}</tbody></table></div>{/if}
<section class="section"><h2>Create a project</h2><form class="panel form stack" onsubmit={(event) => { event.preventDefault(); create(); }}><label>Name<input required bind:value={name} /></label><label>Team<input required bind:value={team} /></label><label>Budget in tokens (optional)<input type="number" min="0" step="1" bind:value={budget} /></label><p class="muted">The server applies team caps and policy. Budgets are allowances, not hard spending ceilings.</p><div><Button type="submit" disabled={busy}>Create project</Button></div></form></section></div>
