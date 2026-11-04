<script lang="ts">
  import { onMount } from 'svelte';
  import { Button } from 'm3-svelte';
  import { api, errorMessage, type Project, type UsageHistory } from '$lib/api';

  const today = new Date();
  const endDefault = today.toISOString().slice(0, 10);
  const startDate = new Date(today);
  startDate.setUTCDate(startDate.getUTCDate() - 30);
  let start = startDate.toISOString().slice(0, 10);
  let end = endDefault;
  let team = '';
  let projectId = '';
  let projects: Project[] = [];
  let projectsLoading = true;
  let projectsError = '';
  let loading = false;
  let error = '';
  let history: UsageHistory | undefined;
  let shownTeam = '';
  let shownProjectId = '';

  async function loadProjects() {
    projectsLoading = true;
    projectsError = '';
    try {
      projects = (await api.projects()).projects;
    } catch (e) {
      projectsError = errorMessage(e);
    } finally {
      projectsLoading = false;
    }
  }

  async function loadUsage() {
    const requestedTeam = team.trim();
    if (!requestedTeam) {
      error = 'Enter a team name.';
      return;
    }
    if (start >= end) {
      error = 'The end date must be after the start date.';
      return;
    }
    if (Date.parse(`${end}T00:00:00Z`) - Date.parse(`${start}T00:00:00Z`) > 366 * 86400000) {
      error = 'The date range must not exceed 366 days.';
      return;
    }
    const requestedProject = projectId;
    loading = true;
    error = '';
    history = undefined;
    try {
      history = await api.usage(requestedTeam, start, end, requestedProject || undefined);
      shownTeam = requestedTeam;
      shownProjectId = requestedProject;
    } catch (e) {
      error = errorMessage(e);
    } finally {
      loading = false;
    }
  }

  function timestamp(value: string | null | undefined) {
    if (!value) return 'Unknown';
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? 'Unknown' : date.toLocaleString();
  }
  function tokens(value: number | null | undefined) {
    return value == null ? 'Unknown' : value.toLocaleString();
  }
  function projectName(id: string) {
    return projects.find(project => project.id === id && project.team === shownTeam)?.name ?? id;
  }

  onMount(loadProjects);
</script>

<div class="page">
  <div class="heading"><div><h1>Usage history</h1><p class="subtitle">Daily usage by team, project, service and traffic class.</p></div></div>
  <p class="muted">These rollups are approximate and for display only; they are not used for quota enforcement. Missing or unknown values are not zero. Recent data may be incomplete.</p>
  {#if projectsLoading}<p role="status">Loading projects…</p>{/if}
  {#if projectsError}<div class="error" role="alert">Could not load project choices: {projectsError} <button type="button" onclick={loadProjects}>Retry</button></div>{/if}
  <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); loadUsage(); }}>
    <label>Team <input required bind:value={team} list="usage-teams" oninput={() => { projectId = ''; }} /></label>
    <datalist id="usage-teams">{#each [...new Set(projects.map(project => project.team))] as name}<option value={name}></option>{/each}</datalist>
    <label>Project <select bind:value={projectId}><option value="">All projects</option>{#each projects.filter(project => project.team === team) as project}<option value={project.id}>{project.name}</option>{/each}</select></label>
    <label>Start date (inclusive, UTC) <input type="date" required bind:value={start} /></label>
    <label>End date (exclusive, UTC) <input type="date" required bind:value={end} /></label>
    <div><Button type="submit" disabled={loading}>Show usage</Button></div>
  </form>
  {#if loading}<p role="status">Loading usage history…</p>{/if}
  {#if error}<div class="error" role="alert">{error}</div>{/if}
  {#if history && !loading}
    <section class="section" aria-labelledby="usage-results">
      <h2 id="usage-results">Usage for {shownTeam}{#if shownProjectId} / {projectName(shownProjectId)}{/if}</h2>
      <p class="muted">{history.start} through {history.end} (end exclusive, UTC). Values are in {history.unit}.</p>
      {#if !history.usage.length}
        <p>No usage rollups returned for this period. This does not establish zero usage.</p>
      {:else}
        <div class="table-wrap"><table><thead><tr><th scope="col">Day (UTC)</th><th scope="col">Project</th><th scope="col">Service</th><th scope="col">Traffic class</th><th scope="col">Input tokens</th><th scope="col">Output tokens</th><th scope="col">Observed</th><th scope="col">Updated</th></tr></thead><tbody>
          {#each history.usage as row}<tr><td>{row.day}</td><td>{projectName(row.project_id)}</td><td>{row.service}</td><td>{row.traffic_class}</td><td>{tokens(row.input_tokens)}</td><td>{tokens(row.output_tokens)}</td><td>{timestamp(row.observed_at)}</td><td>{timestamp(row.updated_at)}</td></tr>{/each}
        </tbody></table></div>
      {/if}
    </section>
  {/if}
</div>
