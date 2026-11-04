<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { afterNavigate, replaceState } from '$app/navigation';
  import { Button } from 'm3-svelte';
  import { ApiError, errorMessage } from '$lib/api';
  import {
    debugApi, type DebugCreateRequest, type DebugEvent, type DebugProject,
    type DebugRun, type DebugService, type DebugSession
  } from '$lib/DebugApi';

  const pendingStorageKey = 'thalamus.debug.pending-create';
  const eventPageSize = 100;
  const maxEventPagesPerPoll = 5;
  const freshnessLimitMs = 10_000;
  const terminalStates = ['completed', 'failed', 'cancelled', 'timed_out'];

  let services: DebugService[] = [];
  let projects: DebugProject[] = [];
  let linkedRun: DebugRun | undefined;
  let serviceId = '';
  let deployment = '';
  let runId = '';
  let projectId = '';
  let model = '';
  let models: string[] = [];
  let modelProject = '';
  let modelsLoading = false;
  let modelsLoaded = false;
  let modelsError = '';
  let selectedService: DebugService | undefined;
  let contextError = '';
  let optionsLoading = true;
  let optionsLoaded = false;
  let optionsError = '';
  let resumeError = '';
  let reconnecting = false;
  let creating = false;
  let actionError = '';
  let pendingCreate: DebugCreateRequest | undefined;
  let retryStorageWarning = '';

  let session: DebugSession | undefined;
  let events: DebugEvent[] = [];
  let cursor = 0;
  let moreEvents = false;
  let lastSync = 0;
  let pollStartedAt = 0;
  let pollError = '';
  let pollFailures = 0;
  let pollPaused = false;
  let polling = false;
  let replying = false;
  let ending = false;
  let replyText = '';
  let pendingReply: { text: string; client_request_key: string } | undefined;
  let now = Date.now();
  let deadlineTime = Number.NaN;
  let deadlinePassed = false;
  let syncStale = false;
  let canReply = false;
  let canEnd = false;

  let mounted = false;
  let lifetime: AbortController;
  let modelsController: AbortController | undefined;
  let modelsVersion = 0;
  let optionsVersion = 0;
  let navigationVersion = 0;
  let observedUrl = '';
  let sessionActions: AbortController | undefined;
  let sessionActionVersion = 0;
  let pollController: AbortController | undefined;
  let pollTimer: ReturnType<typeof setTimeout> | undefined;
  let clockTimer: ReturnType<typeof setInterval> | undefined;
  let pollVersion = 0;

  function hasLinkedProject(run: DebugRun, availableProjects: DebugProject[]): boolean {
    return !!run.project_id && availableProjects.some(project => project.id === run.project_id);
  }

  $: selectedService = services.find(item => item.name === serviceId);
  $: contextError = !optionsLoaded ? ''
    : !selectedService && serviceId ? 'The requested service is not in your accessible service catalog.'
    : deployment && selectedService && !selectedService.deployments.some(item => item.name === deployment)
      ? 'The requested deployment is not part of this service.'
    : linkedRun && (linkedRun.service !== serviceId || linkedRun.deployment !== deployment)
      ? 'The service and deployment do not match the linked run.'
    : linkedRun && !hasLinkedProject(linkedRun, projects)
      ? 'The linked run’s project is unavailable to you.'
    : linkedRun && projectId !== linkedRun.project_id
      ? 'The project does not match the linked run.'
    : linkedRun && !['failed', 'ingestion_failed'].includes(linkedRun.state)
      ? `The linked run is ${linkedRun.state}; only failed runs can supply failure evidence.`
    : '';
  $: deadlineTime = session ? Date.parse(session.deadline) : Number.NaN;
  $: deadlinePassed = Number.isFinite(deadlineTime) && now >= deadlineTime;
  $: syncStale = !!session && ['pending', 'running'].includes(session.state)
    && (lastSync || pollStartedAt) > 0
    && (now + freshnessLimitMs < (lastSync || pollStartedAt) || now - (lastSync || pollStartedAt) > freshnessLimitMs);
  $: canReply = !!session && session.state === 'running' && !!session.upstream_session_id && session.seed_accepted
    && Number.isFinite(deadlineTime) && !deadlinePassed && lastSync > 0
    && !syncStale && !pollError && !replying && !ending && session.tool_calls_used < session.tool_budget;
  $: canEnd = !!session && ['pending', 'running'].includes(session.state) && !ending;

  function isAbort(error: unknown) {
    return error instanceof Error && error.name === 'AbortError';
  }

  function timestamp(value: string | number | null | undefined) {
    if (value == null || value === '') return 'Unknown';
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? 'Unknown' : date.toLocaleString();
  }

  function readPending(): DebugCreateRequest | undefined {
    try {
      const value: unknown = JSON.parse(sessionStorage.getItem(pendingStorageKey) || 'null');
      if (!value || typeof value !== 'object') return undefined;
      const item = value as Record<string, unknown>;
      if (typeof item.project_id !== 'string' || typeof item.service_id !== 'string'
        || typeof item.model !== 'string' || typeof item.client_request_key !== 'string'
        || !(item.deployment == null || typeof item.deployment === 'string')
        || !(item.failed_run_id == null || typeof item.failed_run_id === 'string')) return undefined;
      return {
        project_id: item.project_id, service_id: item.service_id,
        deployment: item.deployment as string | null | undefined,
        failed_run_id: item.failed_run_id as string | null | undefined,
        model: item.model, client_request_key: item.client_request_key
      };
    } catch {
      return undefined;
    }
  }

  // This record contains only identifiers and the idempotency key, never a prompt or credential.
  function rememberPending(input: DebugCreateRequest) {
    pendingCreate = input;
    try {
      sessionStorage.setItem(pendingStorageKey, JSON.stringify(input));
      retryStorageWarning = '';
    } catch {
      retryStorageWarning = 'The browser could not retain the creation retry key across a reload. Keep this page open if you need to retry.';
    }
  }

  function forgetPending(input: DebugCreateRequest) {
    if (pendingCreate?.client_request_key === input.client_request_key) pendingCreate = undefined;
    try {
      if (readPending()?.client_request_key === input.client_request_key) sessionStorage.removeItem(pendingStorageKey);
    } catch {
      // A successful session is still recoverable from its URL.
    }
  }

  function sameCreate(a: DebugCreateRequest, b: Omit<DebugCreateRequest, 'client_request_key'>) {
    return a.project_id === b.project_id && a.service_id === b.service_id && a.model === b.model
      && (a.deployment || '') === (b.deployment || '') && (a.failed_run_id || '') === (b.failed_run_id || '');
  }

  async function loadOptions() {
    const version = ++optionsVersion;
    optionsLoading = true;
    optionsError = '';
    optionsLoaded = false;
    try {
      const [serviceList, projectList, run] = await Promise.all([
        debugApi.services(lifetime.signal), debugApi.projects(lifetime.signal),
        runId ? debugApi.run(runId, lifetime.signal) : Promise.resolve(undefined)
      ]);
      if (!mounted || version !== optionsVersion) return;
      services = serviceList.services;
      projects = projectList.projects;
      linkedRun = run;
      if (run) {
        if (projectId && projectId !== run.project_id) model = '';
        projectId = run.project_id || '';
        if (!serviceId) serviceId = run.service;
        if (!deployment) deployment = run.deployment;
      }
      optionsLoaded = true;
      if (projectId && projects.some(project => project.id === projectId)) void loadModels(projectId, true);
    } catch (error) {
      if (mounted && version === optionsVersion && !isAbort(error)) optionsError = errorMessage(error);
    } finally {
      if (mounted && version === optionsVersion) optionsLoading = false;
    }
  }

  async function loadModels(selectedProject: string, preserveSelection = false) {
    const version = ++modelsVersion;
    modelsController?.abort();
    models = [];
    modelProject = selectedProject;
    modelsLoaded = false;
    modelsError = '';
    if (!preserveSelection) model = '';
    if (!selectedProject) {
      modelsLoading = false;
      return;
    }
    const controller = new AbortController();
    modelsController = controller;
    modelsLoading = true;
    try {
      const result = await debugApi.models(selectedProject, controller.signal);
      if (!mounted || version !== modelsVersion) return;
      models = result.models;
      if (!models.includes(model)) model = '';
      modelsLoaded = true;
    } catch (error) {
      if (mounted && version === modelsVersion && !isAbort(error)) modelsError = errorMessage(error);
    } finally {
      if (mounted && version === modelsVersion) {
        modelsLoading = false;
        if (modelsController === controller) modelsController = undefined;
      }
    }
  }

  function updateSessionUrl(value: DebugSession) {
    const url = new URL(page.url);
    url.searchParams.set('session_id', value.id);
    url.searchParams.set('team_id', value.team_id);
    url.searchParams.set('service', value.service_id);
    if (deployment) url.searchParams.set('deployment', deployment);
    else url.searchParams.delete('deployment');
    if (runId) url.searchParams.set('run_id', runId);
    else url.searchParams.delete('run_id');
    observedUrl = url.pathname + url.search;
    replaceState(url, page.state);
  }

  function stopPolling() {
    pollVersion += 1;
    if (pollTimer !== undefined) clearTimeout(pollTimer);
    pollTimer = undefined;
    pollController?.abort();
    pollController = undefined;
    polling = false;
    pollStartedAt = 0;
  }
  function stopSessionActions() {
    sessionActionVersion += 1;
    sessionActions?.abort();
    sessionActions = undefined;
    replying = false;
    ending = false;
  }

  function beginSessionActions() {
    stopSessionActions();
    sessionActions = new AbortController();
  }


  function schedulePoll(version: number, delay: number) {
    if (!mounted || version !== pollVersion) return;
    pollTimer = setTimeout(() => {
      pollTimer = undefined;
      void poll(version);
    }, delay);
  }

  function startPolling() {
    stopPolling();
    pollFailures = 0;
    pollPaused = false;
    if (mounted && session) void poll(pollVersion);
  }

  async function poll(version: number) {
    const current = session;
    if (!mounted || !current || version !== pollVersion) return;
    const controller = new AbortController();
    pollController = controller;
    polling = true;
    pollStartedAt = Date.now();
    try {
      const latest = await debugApi.session(current.id, current.team_id, controller.signal);
      if (!mounted || version !== pollVersion) return;
      session = latest;
      let more = false;
      for (let i = 0; i < maxEventPagesPerPoll; i += 1) {
        const batch = await debugApi.events(current.id, current.team_id, cursor, eventPageSize, controller.signal);
        if (!mounted || version !== pollVersion) return;
        let next = cursor;
        for (const event of batch) {
          if (event.sequence <= next) throw new Error('The server returned an invalid debug event cursor.');
          next = event.sequence;
        }
        if (batch.length) {
          events = [...events, ...batch];
          cursor = next;
        }
        more = batch.length === eventPageSize;
        if (!more) break;
      }
      lastSync = Date.now();
      pollError = '';
      pollFailures = 0;
      pollPaused = false;
      moreEvents = more;
      const deadline = Date.parse(latest.deadline);
      const active = ['pending', 'running'].includes(latest.state) && Number.isFinite(deadline) && deadline > Date.now();
      if (active) schedulePoll(version, more ? 0 : Math.min(2000, deadline - Date.now()));
    } catch (error) {
      if (!mounted || version !== pollVersion || isAbort(error)) return;
      pollError = errorMessage(error);
      pollFailures += 1;
      const inaccessible = error instanceof ApiError && [401, 403, 404].includes(error.status);
      const deadline = session ? Date.parse(session.deadline) : Number.NaN;
      const active = !!session && ['pending', 'running'].includes(session.state)
        && Number.isFinite(deadline) && deadline > Date.now();
      pollPaused = inaccessible || pollFailures >= 5 || !active;
      if (!pollPaused) schedulePoll(version, Math.min(5000, deadline - Date.now()));
    } finally {
      if (mounted && version === pollVersion) {
        polling = false;
        pollStartedAt = 0;
        if (pollController === controller) pollController = undefined;
      }
    }
  }

  async function reopen(id: string, teamId: string) {
    const version = navigationVersion;
    reconnecting = true;
    resumeError = '';
    try {
      const result = await debugApi.session(id, teamId, lifetime.signal);
      if (!mounted || version !== navigationVersion) return;
      beginSessionActions();
      session = result;
      serviceId = result.service_id;
      startPolling();
    } catch (error) {
      if (mounted && version === navigationVersion && !isAbort(error)) resumeError = errorMessage(error);
    } finally {
      if (mounted && version === navigationVersion) reconnecting = false;
    }
  }

  async function createSession() {
    if (!optionsLoaded || contextError || !projectId || !serviceId || !model || creating
      || !modelsLoaded || modelProject !== projectId || !models.includes(model)) return;
    actionError = '';
    const choice = {
      project_id: projectId, service_id: serviceId, deployment: deployment || null,
      failed_run_id: runId || null, model
    };
    if (pendingCreate && !sameCreate(pendingCreate, choice)
      && !confirm('A previous creation attempt may have succeeded. Changing these choices can create another session. Continue?')) return;
    let input: DebugCreateRequest;
    try {
      input = { ...choice, client_request_key: pendingCreate && sameCreate(pendingCreate, choice)
        ? pendingCreate.client_request_key : crypto.randomUUID() };
    } catch {
      actionError = 'A secure request identifier could not be generated.';
      return;
    }
    rememberPending(input);
    creating = true;
    const version = navigationVersion;
    try {
      const result = await debugApi.create(input, lifetime.signal);
      if (!mounted || version !== navigationVersion) return;
      beginSessionActions();
      session = result;
      events = [];
      cursor = 0;
      moreEvents = false;
      lastSync = 0;
      pollError = '';
      updateSessionUrl(result);
      forgetPending(input);
      startPolling();
    } catch (error) {
      if (mounted && version === navigationVersion && !isAbort(error)) actionError = `${errorMessage(error)} Retry unchanged to use the same request key.`;
    } finally {
      if (mounted && version === navigationVersion) creating = false;
    }
  }

  async function sendReply() {
    const time = Date.now();
    if (!session || !canReply || time >= Date.parse(session.deadline)
      || time < lastSync || time - lastSync > freshnessLimitMs) return;
    const current = session;
    const version = sessionActionVersion;
    const controller = sessionActions;
    if (!controller) return;
    const text = pendingReply?.text ?? replyText;
    if (!text.trim()) return;
    actionError = '';
    try {
      if (!pendingReply) pendingReply = { text, client_request_key: crypto.randomUUID() };
    } catch {
      actionError = 'A secure reply identifier could not be generated.';
      return;
    }
    replying = true;
    try {
      await debugApi.reply(current.id, current.team_id, pendingReply, controller.signal);
      if (!mounted || version !== sessionActionVersion || session?.id !== current.id) return;
      pendingReply = undefined;
      replyText = '';
      startPolling();
    } catch (error) {
      if (mounted && version === sessionActionVersion && session?.id === current.id && !isAbort(error)) {
        actionError = `${errorMessage(error)} Delivery may be uncertain. Check the events; retrying this reply uses the same request key.`;
        startPolling();
      }
    } finally {
      if (mounted && version === sessionActionVersion && session?.id === current.id) replying = false;
    }
  }

  function discardReply() {
    if (confirm('The reply may already have been delivered. Discard the local draft anyway?')) {
      pendingReply = undefined;
      replyText = '';
      actionError = '';
    }
  }

  async function endSession() {
    if (!session || !canEnd || !confirm('End this debug session? Resource cleanup may continue after it stops.')) return;
    const current = session;
    const version = sessionActionVersion;
    const controller = sessionActions;
    if (!controller) return;
    actionError = '';
    ending = true;
    try {
      await debugApi.end(current.id, current.team_id, controller.signal);
      if (mounted && version === sessionActionVersion && session?.id === current.id) startPolling();
    } catch (error) {
      if (mounted && version === sessionActionVersion && session?.id === current.id && !isAbort(error)) {
        actionError = errorMessage(error);
        startPolling();
      }
    } finally {
      if (mounted && version === sessionActionVersion && session?.id === current.id) ending = false;
    }
  }

  function startAnother() {
    stopPolling();
    stopSessionActions();
    session = undefined;
    events = [];
    cursor = 0;
    moreEvents = false;
    lastSync = 0;
    pollError = '';
    actionError = '';
    pendingReply = undefined;
    replyText = '';
    const url = new URL(page.url);
    url.searchParams.delete('session_id');
    url.searchParams.delete('team_id');
    observedUrl = url.pathname + url.search;
    replaceState(url, page.state);
  }

  function runLink(id: string) {
    return `/runs?${new URLSearchParams({ run_id: id })}`;
  }

  function eventLabel(event: DebugEvent) {
    const payload = event.payload;
    if (event.kind === 'opencode.event' && payload && typeof payload === 'object'
      && 'type' in payload && typeof payload.type === 'string') return payload.type;
    return event.kind;
  }

  function eventContent(event: DebugEvent) {
    const payload = event.payload;
    if (payload && typeof payload === 'object') {
      if (event.kind === 'reply.requested' && 'text' in payload && typeof payload.text === 'string') return payload.text;
      if (event.kind === 'opencode.event' && 'properties' in payload && payload.properties
        && typeof payload.properties === 'object' && 'part' in payload.properties
        && payload.properties.part && typeof payload.properties.part === 'object'
        && 'text' in payload.properties.part && typeof payload.properties.part.text === 'string') {
        return payload.properties.part.text;
      }
    }
    return JSON.stringify(payload, null, 2) ?? 'No event payload';
  }

  function initializeRoute(url: URL) {
    navigationVersion += 1;
    optionsVersion += 1;
    lifetime.abort();
    lifetime = new AbortController();
    modelsVersion += 1;
    modelsController?.abort();
    modelsController = undefined;
    stopPolling();
    stopSessionActions();
    observedUrl = url.pathname + url.search;

    services = [];
    projects = [];
    linkedRun = undefined;
    projectId = '';
    model = '';
    models = [];
    modelProject = '';
    modelsLoading = false;
    modelsLoaded = false;
    modelsError = '';
    optionsLoading = true;
    optionsLoaded = false;
    optionsError = '';
    resumeError = '';
    reconnecting = false;
    creating = false;
    actionError = '';
    retryStorageWarning = '';
    session = undefined;
    events = [];
    cursor = 0;
    moreEvents = false;
    lastSync = 0;
    pollError = '';
    pollFailures = 0;
    pollPaused = false;
    pendingReply = undefined;
    replyText = '';
    now = Date.now();

    const query = url.searchParams;
    serviceId = query.get('service') || '';
    deployment = query.get('deployment') || '';
    runId = query.get('run_id') || '';
    const sessionId = query.get('session_id');
    const teamId = query.get('team_id');
    pendingCreate = readPending();
    if (!sessionId && !teamId && pendingCreate
      && (!serviceId || serviceId === pendingCreate.service_id)
      && (!deployment || deployment === pendingCreate.deployment)
      && ((!runId && !pendingCreate.failed_run_id) || runId === pendingCreate.failed_run_id)) {
      serviceId ||= pendingCreate.service_id;
      deployment ||= pendingCreate.deployment || '';
      runId ||= pendingCreate.failed_run_id || '';
      projectId = pendingCreate.project_id;
      model = pendingCreate.model;
    }
    void loadOptions();
    if (sessionId && teamId) void reopen(sessionId, teamId);
    else if (sessionId || teamId) resumeError = 'Both the session ID and team ID are required to reopen a debug session.';
  }

  afterNavigate(({ to }) => {
    if (mounted && to?.route.id === '/debug' && to.url.pathname + to.url.search !== observedUrl) {
      initializeRoute(to.url);
    }
  });

  onMount(() => {
    mounted = true;
    lifetime = new AbortController();
    initializeRoute(page.url);
    clockTimer = setInterval(() => { now = Date.now(); }, 1000);
    return () => {
      mounted = false;
      navigationVersion += 1;
      optionsVersion += 1;
      lifetime.abort();
      modelsVersion += 1;
      modelsController?.abort();
      stopPolling();
      stopSessionActions();
      if (clockTimer !== undefined) clearInterval(clockTimer);
    };
  });
</script>

<div class="page">
  <div class="heading"><div><h1>Debug sessions</h1><p class="subtitle">Investigate a service or failed run with an agent using your own model.</p></div></div>

  {#if resumeError}
    <div class="error" role="alert">Could not reopen the debug session: {resumeError} <a href="/debug">Start a new session</a></div>
  {:else if reconnecting}
    <p role="status">Reopening debug session…</p>
  {:else if !session}
    {#if optionsLoading}<p role="status">Loading available services and projects…</p>{/if}
    {#if optionsError}<div class="error" role="alert">Could not load debug choices: {optionsError} <button type="button" onclick={loadOptions}>Retry</button></div>{/if}
    {#if pendingCreate}<div class="notice">A previous creation attempt may have succeeded. Retry with the same choices to reuse its request key and avoid creating a duplicate session.</div>{/if}
    {#if retryStorageWarning}<div class="error" role="alert">{retryStorageWarning}</div>{/if}
    {#if contextError}<div class="error" role="alert">{contextError}</div>{/if}
    {#if actionError}<div class="error" role="alert">{actionError}</div>{/if}
    {#if optionsLoaded}
      {#if !services.length}<p>No accessible services were returned.</p>{/if}
      {#if !projects.length}<p>No accessible projects were returned. <a href="/projects">Manage projects</a></p>{/if}
      <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); void createSession(); }}>
        <label>Project<select required bind:value={projectId} disabled={creating || !!linkedRun} onchange={(event) => { void loadModels(event.currentTarget.value); }}><option value="" disabled>Select a project</option>{#each projects as project}<option value={project.id}>{project.name} · {project.team}</option>{/each}</select></label>
        <label>Service to investigate<select required bind:value={serviceId} disabled={creating || !!linkedRun} onchange={() => { deployment = ''; }}><option value="" disabled>Select a service</option>{#each services as item}<option value={item.name}>{item.name}</option>{/each}</select></label>
        <label>Deployment (optional)<select bind:value={deployment} disabled={creating || !!linkedRun || !selectedService}><option value="">All deployments</option>{#each selectedService?.deployments ?? [] as item}<option value={item.name}>{item.name}</option>{/each}</select></label>
        {#if linkedRun}<p class="muted">Failure evidence from run <a href={runLink(linkedRun.id)}>{linkedRun.id}</a> ({linkedRun.state}). The project, service and deployment are fixed to this run.</p>{/if}
        <label>Agent model<select required bind:value={model} disabled={creating || !projectId || modelsLoading || !modelsLoaded}><option value="" disabled>{projectId ? 'Select an available model' : 'Select a project first'}</option>{#each models as item}<option value={item}>{item}</option>{/each}</select></label>
        {#if modelsLoading}<p role="status">Loading available models…</p>{/if}
        {#if modelsError}<div class="error" role="alert">Could not load models for this project: {modelsError} <button type="button" onclick={() => loadModels(projectId, true)}>Retry</button></div>{/if}
        {#if modelsLoaded && !models.length}<p>No debug models are available to this project.</p>{/if}
        <p class="muted">The model can differ from the service being investigated. The server verifies access and sets the session deadline and model and tool budgets.</p>
        <div><Button type="submit" disabled={creating || !!contextError || !projects.length || !services.length || !modelsLoaded || modelProject !== projectId || !models.includes(model)}>{creating ? 'Creating…' : 'Create debug session'}</Button></div>
      </form>
    {/if}
  {:else}
    <section class="panel stack" aria-labelledby="session-heading">
      <div class="heading"><div><h2 id="session-heading">Session {session.id}</h2><p class="muted">{session.service_id} · model {session.model}</p></div><div class="actions"><Button variant="outlined" onclick={startPolling} disabled={polling && !syncStale}>Refresh</Button>{#if ['pending', 'running'].includes(session.state)}<Button variant="outlined" onclick={endSession} disabled={!canEnd}>{ending ? 'Ending…' : 'End session'}</Button>{/if}</div></div>
      {#if pollError}<div class="error" role="alert">Session state and events may be stale: {pollError}. {pollPaused ? 'Automatic refresh is paused; use Refresh to retry.' : 'Retrying automatically.'}</div>{/if}
      {#if syncStale && !pollError}<div class="error" role="status">Session updates are stale or disconnected. Replies are disabled; use Refresh to retry.</div>{/if}
      {#if actionError}<div class="error" role="alert">{actionError}</div>{/if}
      {#if deadlinePassed && !terminalStates.includes(session.state)}<div class="error" role="status">The deadline has passed. Replies and automatic refresh are stopped; use Refresh to check the server state.</div>{/if}
      {#if !Number.isFinite(deadlineTime)}<div class="error" role="alert">The session deadline is unknown; replies are disabled.</div>{/if}
      {#if !['pending', 'running', ...terminalStates].includes(session.state)}<div class="error" role="status">The session state is unrecognized. Replies and automatic refresh are stopped.</div>{/if}
      <div class="grid">
        <div><strong>State</strong><span class="detail">{session.state}{pollError || syncStale ? ' · Stale' : ''}</span></div>
        <div><strong>Deadline</strong><span class="detail">{timestamp(session.deadline)}</span></div>
        <div><strong>Model budget</strong><span class="detail">{session.model_budget.toLocaleString()} tokens</span></div>
        <div><strong>Tool budget</strong><span class="detail">{session.tool_calls_used.toLocaleString()} / {session.tool_budget.toLocaleString()} calls used</span></div>
      </div>
      <p class="muted">Last refreshed: {timestamp(lastSync || null)}. {session.cleanup_required ? 'Resource cleanup is pending; refresh to check its progress.' : terminalStates.includes(session.state) ? 'Resource cleanup is complete.' : ['pending', 'running'].includes(session.state) ? 'Session resources are active or starting.' : 'Session resource state is unknown.'}</p>
      {#if session.outcome != null}<div><strong>Outcome</strong><pre>{JSON.stringify(session.outcome, null, 2)}</pre></div>{/if}
      {#if session.state === 'running' && session.tool_calls_used >= session.tool_budget}<p class="muted">The tool budget is exhausted; replies are disabled.</p>{/if}
      {#if terminalStates.includes(session.state) && !session.cleanup_required}<div><Button variant="outlined" onclick={startAnother}>Start another session</Button></div>{/if}
    </section>

    <section class="section" aria-labelledby="events-heading">
      <h2 id="events-heading">Session events</h2>
      <p class="muted">Events are read from the authenticated session transcript. A reported stream gap means the transcript may be incomplete.</p>
      {#if moreEvents && (deadlinePassed || !['pending', 'running'].includes(session.state))}<p class="muted">More transcript events may be available. Refresh to load the next page.</p>{/if}
      {#if !events.length}
        <p>{lastSync ? 'No events have been returned.' : polling ? 'Loading events…' : 'Events have not been loaded.'}</p>
      {:else}
        <div class="stack" aria-live="polite">{#each events as event (event.sequence)}<article class="panel"><strong>{eventLabel(event)}</strong><span class="detail">Event {event.sequence} · {timestamp(event.created_at)}</span><pre>{eventContent(event)}</pre></article>{/each}</div>
      {/if}
    </section>

    {#if session.state === 'pending' || session.state === 'running'}
      <section class="section" aria-labelledby="reply-heading">
        <h2 id="reply-heading">Reply</h2>
        {#if session.state === 'pending'}<p class="muted">Replies are available when the session is running.</p>{/if}
        {#if session.state === 'running' && !session.upstream_session_id}<p class="muted">Waiting for the agent session to initialize.</p>{/if}
        {#if session.state === 'running' && session.upstream_session_id && !session.seed_accepted}<p class="muted">Waiting for the agent to accept the seeded service and failure context.</p>{/if}
        {#if pendingReply}<p class="muted">This reply may have been delivered. Review the events before retrying or discarding the local draft.</p>{/if}
        <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); void sendReply(); }}>
          <label>Message<textarea required autocomplete="off" bind:value={replyText} disabled={!canReply || !!pendingReply}></textarea></label>
          <div class="actions"><Button type="submit" disabled={!canReply || !(pendingReply?.text ?? replyText).trim()}>{replying ? 'Sending…' : pendingReply ? 'Retry reply' : 'Send reply'}</Button>{#if pendingReply}<Button variant="text" onclick={discardReply} disabled={replying}>Discard local draft</Button>{/if}</div>
        </form>
      </section>
    {/if}
  {/if}
</div>
