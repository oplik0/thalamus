<script lang="ts">
  import { onMount } from 'svelte';
  import { Button } from 'm3-svelte';
  import { api, ApiError, errorMessage, type Project } from '$lib/api';
  import { projectTrafficApi } from '$lib/ProjectTrafficApi';

  let projects: Project[] = [];
  let loading = true;
  let loaded = false;
  let busy = false;
  let error = '';
  let name = '';
  let team = '';
  let costCentre = '';
  let budget: number | undefined;
  let classOptions: Record<string, string[]> = {};
  let classSelection: Record<string, string> = {};
  let classesLoading = false;
  let classesError = '';
  let updatingClass = '';
  let classNotice = '';
  let loadVersion = 0;
  let disposed = false;
  let optionsController: AbortController | undefined;

  async function loadClasses(items: Project[], version: number, signal: AbortSignal) {
    classesLoading = true;
    try {
      // The endpoint is platform-admin-only. Once it denies the caller there
      // is no need to issue the same denied request for every project.
      for (const project of items) {
        try {
          const { classes } = await projectTrafficApi.classes(project.id, signal);
          if (disposed || signal.aborted || version !== loadVersion) return;
          classOptions = { ...classOptions, [project.id]: classes };
          classSelection = { ...classSelection, [project.id]: classes.includes(project.traffic_class) ? project.traffic_class : '' };
        } catch (e) {
          if (disposed || signal.aborted || version !== loadVersion) return;
          if (e instanceof ApiError && e.status === 403) {
            classOptions = {};
            return;
          }
          if (e instanceof ApiError && e.status === 404) continue;
          classOptions = {};
          classesError = errorMessage(e);
          return;
        }
      }
    } finally {
      if (!disposed && version === loadVersion) classesLoading = false;
    }
  }

  async function load() {
    if (disposed) return;
    const version = ++loadVersion;
    optionsController?.abort();
    optionsController = new AbortController();
    const signal = optionsController.signal;
    classOptions = {};
    classSelection = {};
    classesError = '';
    classesLoading = false;
    loading = true;
    loaded = false;
    error = '';
    try {
      const result = await api.projects();
      if (disposed || version !== loadVersion) return;
      projects = result.projects;
      loaded = true;
      void loadClasses(projects, version, signal);
    } catch (e) {
      if (!disposed && version === loadVersion) error = errorMessage(e);
    } finally {
      if (!disposed && version === loadVersion) loading = false;
    }
  }

  async function create() {
    busy = true;
    error = '';
    try {
      await api.createProject({ name, team, cost_centre: costCentre.trim() || null, budget: budget ?? null });
      if (disposed) return;
      name = '';
      team = '';
      costCentre = '';
      budget = undefined;
      await load();
    } catch (e) {
      if (!disposed) error = errorMessage(e);
    } finally {
      if (!disposed) busy = false;
    }
  }

  async function updateClass(project: Project) {
    const trafficClass = classSelection[project.id];
    if (!trafficClass || !classOptions[project.id]?.includes(trafficClass)) return;
    updatingClass = project.id;
    classNotice = '';
    error = '';
    try {
      await projectTrafficApi.set(project.id, { traffic_class: trafficClass });
      if (disposed) return;
      classNotice = `Requested traffic class updated for ${project.name}.`;
      await load();
    } catch (e) {
      if (!disposed) {
        error = errorMessage(e);
        if (e instanceof ApiError && [403, 404, 503].includes(e.status)) classOptions = {};
      }
    } finally {
      if (!disposed) updatingClass = '';
    }
  }

  onMount(() => {
    void load();
    return () => {
      disposed = true;
      loadVersion++;
      optionsController?.abort();
    };
  });
</script>
<div class="page"><div class="heading"><div><h1>Projects</h1><p class="subtitle">Group keys and attribute usage to the teams they serve.</p></div><Button variant="outlined" onclick={load}>Refresh</Button></div>
{#if error}<div class="error" role="alert">{error}</div>{/if}{#if loading}<p role="status">Loading projects…</p>{:else if loaded && !projects.length}<p class="muted">No projects returned.</p>{:else if loaded}<div class="table-wrap"><table><thead><tr><th>Name</th><th>Team</th><th>Cost centre</th><th>Budget</th><th>Requested traffic class</th></tr></thead><tbody>{#each projects as project}<tr><td>{project.name}</td><td>{project.team}</td><td>{project.cost_centre ?? 'Not set'}</td><td>{project.budget ?? 'Not set'}</td><td>{project.traffic_class}</td></tr>{/each}</tbody></table></div>{/if}
{#if classNotice}<p role="status">{classNotice}</p>{/if}
{#if classesLoading}<p class="muted" role="status">Checking reviewed traffic-class policy…</p>{/if}
{#if classesError}<div class="error" role="alert">Traffic-class policy unavailable: {classesError}</div>{/if}
{#if Object.keys(classOptions).length}
<section class="section">
  <h2>Manage traffic classes</h2>
  <p class="muted">Only classes allowed by the current reviewed team policy are offered. The value above is requested state; gateway key projections reconcile asynchronously.</p>
  {#each projects as project}
    {#if Object.hasOwn(classOptions, project.id)}
      <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); updateClass(project); }}>
        <h3>{project.name} · {project.team}</h3>
        {#if classOptions[project.id].length}
          <label>Reviewed traffic class
            <select value={classSelection[project.id] || ''} onchange={(event) => { classSelection = { ...classSelection, [project.id]: event.currentTarget.value }; }}>
              <option value="" disabled>Select a reviewed class</option>
              {#each classOptions[project.id] as trafficClass}<option value={trafficClass}>{trafficClass}</option>{/each}
            </select>
          </label>
          <div><Button type="submit" disabled={busy || !!updatingClass || !classSelection[project.id] || classSelection[project.id] === project.traffic_class}>Update traffic class</Button></div>
        {:else}
          <p class="muted">No reviewed classes are available for this project's team.</p>
        {/if}
      </form>
    {/if}
  {/each}
</section>
{/if}
<section class="section"><h2>Create a project</h2><form class="panel form stack" onsubmit={(event) => { event.preventDefault(); create(); }}><label>Name<input required bind:value={name} /></label><label>Team<input required bind:value={team} /></label><label>Cost centre (optional)<input maxlength="128" bind:value={costCentre} /></label><label>Budget in tokens (optional)<input type="number" min="0" step="1" bind:value={budget} /></label><p class="muted">The server applies team caps and policy. Budgets are allowances, not hard spending ceilings.</p><div><Button type="submit" disabled={busy}>Create project</Button></div></form></section></div>
