<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { Button } from 'm3-svelte';
  import { api, ApiError, errorMessage, type Project, type Run, type Service } from '$lib/api';
  import { createRun, promoteRun, type RunPromotion } from '$lib/RunApi';
  import BenchmarkResults from '$lib/BenchmarkResults.svelte';

  let runs: Run[] = [];
  let services: Service[] = [];
  let projects: Project[] = [];
  let selected: Run | undefined;
  let selectedId = '';
  let projectId = '';
  let service = '';
  let deployment = '';
  let kind: Parameters<typeof api.createRun>[0]['kind'] = 'validation';
  let error = '';
  let selectionError = '';
  let loading = true;
  let loaded = false;
  let formReady = false;
  let busy = false;
  let inspecting = false;
  let promoting = false;
  let promotion: RunPromotion | undefined;
  let promotionError = '';

  let mounted = false;
  let selectionVersion = 0;
  let loadVersion = 0;
  let pollTimer: ReturnType<typeof setTimeout> | undefined;
  let pendingCreate: { fingerprint: string; key: string } | undefined;

  const activeRun = (run: Run) => ['pending', 'running', 'ingesting'].includes(run.state);

  function stopPolling() {
    if (pollTimer !== undefined) clearTimeout(pollTimer);
    pollTimer = undefined;
  }

  function schedulePoll(run: Run, version: number) {
    stopPolling();
    if (mounted && activeRun(run)) {
      pollTimer = setTimeout(() => void fetchSelection(run.id, version), 5000);
    }
  }

  async function load() {
    const version = ++loadVersion;
    loading = true;
    error = '';
    const [r, s, p] = await Promise.allSettled([api.runs(), api.services(), api.projects()]);
    if (!mounted || version !== loadVersion) return;
    const failures: string[] = [];
    if (r.status === 'fulfilled') {
      runs = r.value.runs;
      loaded = true;
    } else {
      failures.push(`Run list unavailable${loaded ? ' (previous list may be stale)' : ''}: ${errorMessage(r.reason)}`);
    }
    if (s.status === 'fulfilled') services = s.value.services;
    else failures.push(`Services unavailable: ${errorMessage(s.reason)}`);
    if (p.status === 'fulfilled') {
      projects = p.value.projects;
      if (!projects.some(project => project.id === projectId)) {
        projectId = '';
        pendingCreate = undefined;
      }
    } else failures.push(`Projects unavailable: ${errorMessage(p.reason)}`);
    formReady = s.status === 'fulfilled' && p.status === 'fulfilled';
    error = failures.join(' ');
    loading = false;
  }

  async function fetchSelection(id: string, version: number) {
    try {
      const run = await api.run(id);
      if (!mounted || version !== selectionVersion) return;
      selected = run;
      runs = runs.map(item => item.id === run.id ? run : item);
      selectionError = '';
      schedulePoll(run, version);
    } catch (e) {
      if (!mounted || version !== selectionVersion) return;
      selectionError = errorMessage(e);
      if (selected && selected.id === id && !(e instanceof ApiError && [401, 403, 404].includes(e.status))) {
        schedulePoll(selected, version);
      }
    } finally {
      if (mounted && version === selectionVersion) inspecting = false;
    }
  }

  function inspect(id: string) {
    stopPolling();
    const version = ++selectionVersion;
    selectedId = id;
    selected = undefined;
    selectionError = '';
    promotion = undefined;
    promotionError = '';
    inspecting = true;
    void fetchSelection(id, version);
  }

  async function create() {
    busy = true;
    error = '';
    try {
      const body = { project_id: projectId, service, deployment, kind };
      const fingerprint = JSON.stringify(body);
      if (!pendingCreate || pendingCreate.fingerprint !== fingerprint) {
        pendingCreate = { fingerprint, key: crypto.randomUUID() };
      }
      const key = pendingCreate.key;
      const run = await createRun(body, key);
      if (!mounted) return;
      if (pendingCreate?.key === key) pendingCreate = undefined;
      stopPolling();
      const version = ++selectionVersion;
      selectedId = run.id;
      selected = run;
      runs = [run, ...runs.filter(item => item.id !== run.id)];
      selectionError = '';
      inspecting = false;
      promotion = undefined;
      promotionError = '';
      schedulePoll(run, version);
      try {
        const result = await api.runs();
        if (mounted) { runs = result.runs; loaded = true; }
      } catch (e) {
        if (mounted) error = `Run started, but the run list could not be refreshed: ${errorMessage(e)}`;
      }
    } catch (e) {
      if (mounted) error = errorMessage(e);
    } finally {
      if (mounted) busy = false;
    }
  }

  async function promote(run: Run) {
    const version = selectionVersion;
    promoting = true;
    promotion = undefined;
    promotionError = '';
    try {
      const response = await promoteRun(run.id);
      let url: URL;
      try {
        url = new URL(response.pr_url);
      } catch {
        throw new Error('The server returned an invalid pull request URL.');
      }
      if (!['https:', 'http:'].includes(url.protocol)) {
        throw new Error('The server returned an invalid pull request URL.');
      }
      if (mounted && version === selectionVersion) promotion = { ...response, pr_url: url.href };
    } catch (e) {
      if (mounted && version === selectionVersion) promotionError = errorMessage(e);
    } finally {
      if (mounted) promoting = false;
    }
  }

  function debug(run: Run) {
    const query = new URLSearchParams({ service: run.service, deployment: run.deployment });
    if (run.state === 'failed' || run.state === 'ingestion_failed') query.set('run_id', run.id);
    void goto(`/debug?${query}`);
  }

  onMount(() => {
    mounted = true;
    void load();
    const linkedRun = page.url.searchParams.get('run_id');
    if (linkedRun) inspect(linkedRun);
    return () => {
      mounted = false;
      selectionVersion++;
      loadVersion++;
      stopPolling();
    };
  });
</script>

<div class="page">
  <div class="heading">
    <div>
      <h1>Runs</h1>
      <p class="subtitle">Validation and benchmark evidence is bound to a deployment's tested configuration.</p>
    </div>
    <Button variant="outlined" disabled={loading} onclick={load}>Refresh</Button>
  </div>

  {#if error}<div class="error" role="alert">{error}</div>{/if}
  {#if loading && !loaded}
    <p role="status">Loading runs…</p>
  {:else if loaded && !runs.length}
    <p class="muted">No runs returned.</p>
  {:else if loaded}
    <div class="table-wrap">
      <table>
        <thead><tr><th>Run</th><th>Service</th><th>Deployment</th><th>Kind</th><th>Configuration identity</th><th>State</th><th>Action</th></tr></thead>
        <tbody>
          {#each runs as run (run.id)}
            <tr>
              <td>{run.id}</td><td>{run.service}</td><td>{run.deployment}</td><td>{run.kind}</td>
              <td>{run.revision}</td><td>{run.state}</td>
              <td><Button variant="text" disabled={busy || promoting} onclick={() => inspect(run.id)}>Inspect</Button></td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}

  {#if inspecting}
    <section class="section"><p role="status">Loading run {selectedId}…</p></section>
  {/if}
  {#if selectionError}
    <div class="error" role="alert">Could not refresh run {selectedId}: {selectionError}</div>
    <Button variant="outlined" disabled={busy || promoting} onclick={() => inspect(selectedId)}>Retry run</Button>
  {/if}
  {#if selected}
    <section class="section">
      <h2>Run {selected.id}</h2>
      <p>{selected.kind} · {selected.service} / {selected.deployment}</p>
      <p>Desired configuration identity: <code>{selected.revision}</code> · Last reported state: {selected.state}{selectionError ? ' (stale; refresh failed)' : ''}</p>
      <div class="actions">
        {#if !selectionError}
          <Button variant="text" disabled={busy || promoting} onclick={() => selected && inspect(selected.id)}>Refresh run</Button>
        {/if}
        <Button variant="outlined" onclick={() => selected && debug(selected)}>
          {selected.state === 'failed' || selected.state === 'ingestion_failed' ? 'Debug failed run' : 'Open debug'}
        </Button>
        {#if selected.state === 'succeeded' && (selected.kind === 'benchmark' || selected.kind === 'validation')}
          <Button disabled={busy || promoting} onclick={() => selected && promote(selected)}>
            {promoting ? 'Preparing candidate pull request…' : 'Promote tested configuration'}
          </Button>
        {/if}
      </div>
      {#if selected.state === 'succeeded' && (selected.kind === 'benchmark' || selected.kind === 'validation')}
        <p class="muted">A platform administrator can open a candidate pull request for an eligible gated deployment. Required functional validation and rollout gates still apply.</p>
      {/if}
      {#if promotionError}<div class="error" role="alert">Could not promote this run: {promotionError}</div>{/if}
      {#if promotion}
        <p role="status">Candidate pull request: <a href={promotion.pr_url} target="_blank" rel="noopener noreferrer">{promotion.pr_url}</a> · Change {promotion.change_id}</p>
      {/if}
      {#if selected.kind === 'benchmark'}
        {#if selected.state === 'succeeded'}
          {#key selected.id}
            <BenchmarkResults run={selected} {runs} />
          {/key}
        {:else if activeRun(selected)}
          <p class="muted" role="status">Benchmark evidence is not available yet. Run status refreshes automatically.</p>
        {:else}
          <p class="muted">No successful benchmark result is available for this run. State: {selected.state}.</p>
        {/if}
      {/if}
    </section>
  {/if}

  <section class="section">
    <h2>Start a run</h2>
    <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); void create(); }}>
      <label>Project
        <select required bind:value={projectId} onchange={() => { pendingCreate = undefined; }}>
          <option value="" disabled>Select a project</option>
          {#each projects as project}<option value={project.id}>{project.name} ({project.team})</option>{/each}
        </select>
      </label>
      {#if formReady && !projects.length}<p class="muted">No projects available. Create a project before starting a run.</p>{/if}
      <label>Service
        <select required bind:value={service} onchange={() => { deployment = ''; pendingCreate = undefined; }}>
          <option value="" disabled>Select a service</option>
          {#each services as item}<option value={item.name}>{item.name}</option>{/each}
        </select>
      </label>
      <label>Deployment
        <select required bind:value={deployment} onchange={() => { pendingCreate = undefined; }}>
          <option value="" disabled>Select a deployment</option>
          {#each services.find(item => item.name === service)?.deployments || [] as item}
            <option value={item.name}>{item.name}</option>
          {/each}
        </select>
      </label>
      <label>Kind
        <select bind:value={kind} onchange={() => { pendingCreate = undefined; }}>
          <option value="validation">Validation</option>
          <option value="benchmark">Benchmark</option>
        </select>
      </label>
      <div><Button type="submit" disabled={busy || promoting || loading || !formReady}>Start run</Button></div>
    </form>
  </section>
</div>
