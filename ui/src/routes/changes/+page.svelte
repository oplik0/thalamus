<script lang="ts">
  import { onMount } from 'svelte';
  import { afterNavigate } from '$app/navigation';
  import { page } from '$app/state';
  import { Button } from 'm3-svelte';
  import { api, ApiError, errorMessage, type Change, type Service, type Validation } from '$lib/api';
  import { createChange, getChangeForm, type ChangeRequest, type ChangeServiceForm } from '$lib/ChangeApi';
  import { buildChangePatch, buildNewArtifactPatch, hasUnsafeChangeNumber, isChangeObject, type ChangeObject } from '$lib/ChangeForm';
  import ChangeDeploymentFields from '$lib/ChangeDeploymentFields.svelte';
  import ChangeRollout from '$lib/ChangeRollout.svelte';

  let changes: Change[] = [];
  let services: Service[] = [];
  let changesLoading = true;
  let servicesLoading = true;
  let changesError = '';
  let servicesError = '';
  let formError = '';
  let actionError = '';
  let service = '';
  let serviceForm: ChangeServiceForm | undefined;
  let draft: ChangeObject | undefined;
  let formLoading = false;
  let deploymentIndex = 0;
  let fieldsValid = true;
  let formVersion = 0;
  let advanced = false;
  let advancedPatch = '';
  let addingNewArtifact = false;
  let artifactName = '';
  let artifactUri = '';
  let artifactModelDigest = '';
  let artifactTokenizerDigest = '';
  let busy = false;
  let selected: Change | undefined;
  let initialSelectionHandled = false;
  let rolloutId = '';
  let rolloutInput = '';
  let rolloutVersion = 0;
  let validation: Validation | undefined;
  let prUrl = '';
  let refreshed = false;
  let attemptKey = '';
  let attemptBody = '';
  let alive = true;
  let changesVersion = 0;
  let servicesVersion = 0;
  let navigationVersion = 0;
  let lastUrlChange: string | undefined;
  let formAbort: AbortController | undefined;
  let proposalAbort: AbortController | undefined;

  function resetAttempt() { attemptKey = ''; attemptBody = ''; }

  function loadLists() {
    void loadChanges();
    void loadServices();
  }

  async function loadServices() {
    const version = ++servicesVersion;
    servicesLoading = true;
    servicesError = '';
    try {
      const result = await api.services();
      if (alive && version === servicesVersion) services = result.services;
    } catch (error) {
      if (alive && version === servicesVersion) servicesError = errorMessage(error);
    } finally {
      if (alive && version === servicesVersion) servicesLoading = false;
    }
  }

  async function loadChanges() {
    const version = ++changesVersion;
    changesLoading = true;
    changesError = '';
    try {
      const result = await api.changes();
      if (!alive || version !== changesVersion) return;
      changes = result.changes;
      if (selected) selected = changes.find(change => change.id === selected?.id) ?? selected;
      if (!initialSelectionHandled) {
        initialSelectionHandled = true;
        const requested = page.url.searchParams.get('change');
        if (requested && requested === rolloutId && !selected) {
          const match = changes.find(change => change.id === requested);
          if (match) selectChange(match);
        }
      }
    } catch (error) {
      if (alive && version === changesVersion) changesError = errorMessage(error);
    } finally {
      if (alive && version === changesVersion) changesLoading = false;
    }
  }

  function invalidateChanges() {
    changesVersion += 1;
    changesLoading = false;
  }

  async function loadForm(name: string) {
    formAbort?.abort();
    const controller = new AbortController();
    formAbort = controller;
    serviceForm = undefined;
    draft = undefined;
    formError = '';
    formLoading = !!name;
    deploymentIndex = 0;
    fieldsValid = true;
    advanced = false;
    advancedPatch = '';
    clearNewArtifact();
    formVersion += 1;
    resetAttempt();
    if (!name) return;
    try {
      const result = await getChangeForm(name, controller.signal);
      if (!alive || controller.signal.aborted) return;
      if (result.service !== name) throw new Error('The server returned a form for a different service.');
      if (!isChangeObject(result.source) || !isChangeObject(result.schema) || !isChangeObject(result.artifacts)) {
        throw new Error('The server returned incomplete committed form data.');
      }
      if (!Array.isArray(result.source.deployments) || !result.source.deployments.length) {
        throw new Error('The committed service has no deployments to edit.');
      }
      serviceForm = result;
      draft = structuredClone(result.source);
    } catch (error) {
      if (alive && !controller.signal.aborted) formError = errorMessage(error);
    } finally {
      if (alive && !controller.signal.aborted) formLoading = false;
    }
  }

  function chooseService(name: string) {
    service = name;
    void loadForm(name);
  }

  function editDraft(next: ChangeObject) {
    draft = next;
    resetAttempt();
  }

  function clearNewArtifact() {
    addingNewArtifact = false;
    artifactName = '';
    artifactUri = '';
    artifactModelDigest = '';
    artifactTokenizerDigest = '';
    resetAttempt();
  }

  function editAdvanced() {
    if (!serviceForm || !draft || !isChangeObject(serviceForm.source)) return;
    try {
      const patch = buildChangePatch(serviceForm.source, draft);
      if (hasUnsafeChangeNumber(patch)) throw new Error('The patch contains a non-finite number or an integer the browser cannot preserve exactly.');
      advancedPatch = JSON.stringify(patch, null, 2);
      advanced = true;
      formError = '';
    } catch (error) {
      formError = errorMessage(error);
    }
  }

  async function propose() {
    if (!serviceForm || !draft || !isChangeObject(serviceForm.source)) return;
    formError = '';
    let patch: ChangeObject;
    let newArtifact: NonNullable<ChangeRequest['new_artifact']> | undefined;
    try {
      const value: unknown = advanced ? JSON.parse(advancedPatch) : buildChangePatch(serviceForm.source, draft);
      if (!isChangeObject(value)) throw new Error('The patch must be a JSON object.');
      if (Object.hasOwn(value, 'name')) throw new Error('The service name cannot be changed.');
      if (addingNewArtifact) {
        const name = artifactName.trim();
        const uri = artifactUri.trim();
        const modelDigest = artifactModelDigest.trim();
        const tokenizerDigest = artifactTokenizerDigest.trim();
        if (!name || !uri.startsWith('oci://') || !uri.slice(6) || !modelDigest || !tokenizerDigest) {
          throw new Error('Enter the new artifact name, OCI URI, and both content digests.');
        }
        if (Object.hasOwn(serviceForm.artifacts, name)) throw new Error('That artifact name already exists in the committed catalog.');
        newArtifact = { name, uri, model_digest: modelDigest, tokenizer_digest: tokenizerDigest };
        patch = buildNewArtifactPatch(serviceForm.source, value, deploymentIndex, newArtifact);
      } else {
        patch = value;
      }
      if (!Object.keys(patch).length) throw new Error('Make at least one change before creating a proposal.');
      if (hasUnsafeChangeNumber(patch)) throw new Error('The patch contains a non-finite number or an integer the browser cannot preserve exactly. It cannot safely replace these catalog values.');
      if (!advanced && !fieldsValid) throw new Error('Correct the highlighted deployment fields first.');
    } catch (error) {
      formError = errorMessage(error);
      return;
    }
    const body: ChangeRequest = { service: serviceForm.service, base_commit: serviceForm.base_commit, patch, ...(newArtifact ? { new_artifact: newArtifact } : {}) };
    const serialized = JSON.stringify(body);
    if (!attemptKey || attemptBody !== serialized) {
      attemptKey = crypto.randomUUID();
      attemptBody = serialized;
    }
    const navigation = navigationVersion;
    invalidateChanges();
    busy = true;
    const controller = new AbortController();
    proposalAbort = controller;
    let timedOut = false;
    const deadline = setTimeout(() => { timedOut = true; controller.abort(); }, 30000);
    try {
      const change = await createChange(body, attemptKey, controller.signal);
      if (!alive) return;
      if (navigation === navigationVersion) selectChange(change);
      resetAttempt();
      await loadChanges();
    } catch (error) {
      if (alive) formError = timedOut
        ? 'Creating the proposal timed out. Retry unchanged to find the same proposal if the request was processed.'
        : explainActionError(error);
    } finally {
      clearTimeout(deadline);
      if (alive) busy = false;
    }
  }

  function selectChange(change: Change, updateRollout = true) {
    selected = change;
    validation = undefined;
    prUrl = '';
    refreshed = false;
    actionError = '';
    if (updateRollout) {
      rolloutId = change.id;
      rolloutInput = change.id;
      rolloutVersion += 1;
    }
  }

  function selectRelatedChange(id: string) {
    const requested = id.trim();
    if (!requested) return;
    const change = changes.find(item => item.id === requested);
    if (change) selectChange(change);
    else {
      selected = undefined;
      validation = undefined;
      prUrl = '';
      refreshed = false;
      actionError = '';
      rolloutId = requested;
      rolloutInput = requested;
      rolloutVersion += 1;
    }
  }

  function selectStageChange(id: string) {
    const change = changes.find(item => item.id === id);
    if (change) selectChange(change, false);
  }

  function explainActionError(error: unknown): string {
    const message = errorMessage(error);
    if (error instanceof ApiError && error.status === 409) {
      return `${message} If the base changed, reload committed data and make a new proposal. If refresh succeeds, validate and review the diffs again.`;
    }
    return message;
  }

  async function refreshChange() {
    if (!selected) return;
    const navigation = navigationVersion;
    const id = selected.id;
    invalidateChanges();
    busy = true;
    actionError = '';
    validation = undefined;
    prUrl = '';
    refreshed = false;
    try {
      const updated = await api.refreshChange(id);
      if (!alive || navigation !== navigationVersion) { if (alive) void loadChanges(); return; }
      selected = updated;
      refreshed = true;
      await loadChanges();
    } catch (error) {
      if (alive && navigation === navigationVersion) actionError = explainActionError(error);
    } finally {
      if (alive) busy = false;
    }
  }

  async function validate() {
    if (!selected) return;
    const navigation = navigationVersion;
    const id = selected.id;
    invalidateChanges();
    busy = true;
    actionError = '';
    validation = undefined;
    prUrl = '';
    try {
      const result = await api.validateChange(id);
      if (!alive || navigation !== navigationVersion) { if (alive) void loadChanges(); return; }
      validation = result;
      refreshed = false;
      await loadChanges();
    } catch (error) {
      if (alive && navigation === navigationVersion) actionError = explainActionError(error);
    } finally {
      if (alive) busy = false;
    }
  }

  async function openPr() {
    if (!selected || !validation || validation.errors.length) return;
    const navigation = navigationVersion;
    const id = selected.id;
    invalidateChanges();
    busy = true;
    actionError = '';
    try {
      const result = await api.openPr(id);
      if (!alive || navigation !== navigationVersion) { if (alive) void loadChanges(); return; }
      const url = new URL(result.url);
      if (!['https:', 'http:'].includes(url.protocol)) throw new Error('The server returned an invalid pull request URL.');
      prUrl = url.href;
      await loadChanges();
    } catch (error) {
      if (alive && navigation === navigationVersion) {
        actionError = explainActionError(error);
        if (error instanceof ApiError && error.status === 409) validation = undefined;
      }
    } finally {
      if (alive) busy = false;
    }
  }

  function deploymentName(value: unknown, index: number): string {
    return isChangeObject(value) && typeof value.name === 'string' && value.name ? value.name : `Unnamed deployment (index ${index})`;
  }

  afterNavigate(({ to }) => {
    const requested = to?.url.searchParams.get('change') ?? '';
    if (requested === lastUrlChange) return;
    lastUrlChange = requested;
    navigationVersion += 1;
    if (requested === rolloutId) return;
    const match = changes.find(change => change.id === requested);
    if (match) selectChange(match);
    else {
      selected = undefined;
      validation = undefined;
      prUrl = '';
      refreshed = false;
      actionError = '';
      rolloutId = requested;
      rolloutInput = requested;
      rolloutVersion += 1;
    }
  });

  onMount(() => {
    void loadLists();
    const initialService = page.url.searchParams.get('service');
    if (initialService) chooseService(initialService);
    return () => {
      alive = false;
      changesVersion += 1;
      servicesVersion += 1;
      formAbort?.abort();
      proposalAbort?.abort();
    };
  });
</script>

<div class="page">
  <div class="heading">
    <div><h1>Changes</h1><p class="subtitle">Edit committed catalog values, validate the rendered change, and open a review request.</p></div>
    <Button variant="outlined" onclick={loadLists} disabled={busy}>Refresh</Button>
  </div>

  <section>
    <h2>Recent changes</h2>
    {#if changesLoading}<p role="status">Loading changes…</p>
    {:else if changesError}<div class="error" role="alert">Could not load changes: {changesError}</div>
    {:else if !changes.length}<p class="muted">No changes returned.</p>
    {:else}
      <div class="table-wrap"><table><thead><tr><th scope="col">Change</th><th scope="col">Service</th><th scope="col">Base commit</th><th scope="col">State</th><th scope="col">Action</th></tr></thead><tbody>
        {#each changes as change (change.id)}
          <tr><td>{change.id}</td><td>{change.service}</td><td><code>{change.base_commit}</code></td><td>{change.state}</td><td><Button variant="text" disabled={busy} onclick={() => selectChange(change)}>{selected?.id === change.id ? 'Selected' : 'Select'}</Button></td></tr>
        {/each}
      </tbody></table></div>
    {/if}
  </section>

  <section class="section">
    <h2>Find a rollout</h2>
    <p class="muted">Service owners can look up a rollout by change ID even when it is not in the recent list.</p>
    <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); selectRelatedChange(rolloutInput); }}>
      <label>Change ID <input required bind:value={rolloutInput} disabled={busy} /></label>
      <div><Button type="submit" disabled={busy}>View rollout</Button></div>
    </form>
    {#if rolloutId}
      <p>Change <code>{rolloutId}</code> · <a href={`/changes?${new URLSearchParams({ change: rolloutId })}`}>Shareable rollout link</a></p>
      {#key `${rolloutId}:${rolloutVersion}`}<ChangeRollout changeId={rolloutId} onSelectChange={selectStageChange} canSelectChange={(id) => changes.some(change => change.id === id)} selectionDisabled={busy} />{/key}
    {/if}
  </section>

  <section class="section">
    <h2>Propose a change</h2>
    <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); void propose(); }}>
      <label>Service
        <select value={service} disabled={busy || servicesLoading} onchange={(event) => chooseService(event.currentTarget.value)}>
          <option value="">Select a service</option>
          {#each services as item (item.name)}<option value={item.name}>{item.name}</option>{/each}
        </select>
      </label>
      {#if servicesLoading}<p role="status">Loading services…</p>{/if}
      {#if servicesError}<div class="error" role="alert">Could not load services: {servicesError}</div>{/if}
      {#if formLoading}<p role="status">Loading the current committed service and KCL schema…</p>{/if}
      {#if formError}<div class="error" role="alert">{formError}</div>{/if}
      {#if service && !formLoading && !serviceForm}
        <div><Button type="button" variant="outlined" disabled={busy} onclick={() => loadForm(service)}>Retry loading committed data</Button></div>
      {/if}
      {#if serviceForm && draft && Array.isArray(draft.deployments)}
        <p class="muted">Committed base: <code>{serviceForm.base_commit}</code>. Unedited values are preserved. KCL render checks remain authoritative.</p>
        <div><Button type="button" variant="outlined" disabled={busy} onclick={() => {
          if (window.confirm('Reload committed data and discard the current form edits?')) void loadForm(service);
        }}>Reload committed data</Button></div>
        {#if !advanced}
          <label>Deployment
            <select value={String(deploymentIndex)} disabled={busy || !fieldsValid} onchange={(event) => { deploymentIndex = Number(event.currentTarget.value); resetAttempt(); }}>
              {#each draft.deployments as deployment, index}<option value={String(index)}>{deploymentName(deployment, index)}</option>{/each}
            </select>
          </label>
          <p class="muted">Fields below come from the committed KCL-generated schema. Only changed catalog values are proposed.</p>
          {#key formVersion}
            <ChangeDeploymentFields schema={serviceForm.schema} base={serviceForm.source as ChangeObject} artifacts={serviceForm.artifacts} {draft} {deploymentIndex} disabled={busy} onChange={editDraft} bind:valid={fieldsValid} />
          {/key}
          <div><Button type="button" variant="text" disabled={busy || !fieldsValid} onclick={editAdvanced}>Advanced JSON patch</Button></div>
        {:else}
          <label>Advanced top-level patch (JSON object)
            <textarea required disabled={busy} spellcheck="false" value={advancedPatch} oninput={(event) => { advancedPatch = event.currentTarget.value; resetAttempt(); }}></textarea>
          </label>
          <p class="muted">Each top-level field replaces that entire field in the committed service, not just the nested keys. Include all deployments and their unchanged values when changing <code>deployments</code>.</p>
          <div><Button type="button" variant="text" disabled={busy} onclick={() => { advanced = false; formError = ''; resetAttempt(); }}>Discard advanced patch and return to form</Button></div>
        {/if}
        {#if addingNewArtifact}
          <div class="stack">
            <h3>New packaged artifact</h3>
            <p class="muted">Provide the real immutable OCI URI and model and tokenizer content digests. The same reviewed PR will add this artifact and set {deploymentName(draft.deployments[deploymentIndex], deploymentIndex)} to use it, its image, and packaged-model mode. These are declared intent, not runtime evidence.</p>
            <label>New artifact name <input required disabled={busy} spellcheck="false" bind:value={artifactName} oninput={resetAttempt} /></label>
            <label>OCI artifact URI <input required disabled={busy} spellcheck="false" bind:value={artifactUri} oninput={resetAttempt} /></label>
            <label>Model content digest <input required disabled={busy} spellcheck="false" bind:value={artifactModelDigest} oninput={resetAttempt} /></label>
            <label>Tokenizer content digest <input required disabled={busy} spellcheck="false" bind:value={artifactTokenizerDigest} oninput={resetAttempt} /></label>
            <p class="muted">The URI must be an <code>oci://</code> reference pinned by SHA-256; both content digests must be <code>sha256:</code> followed by 64 lowercase hexadecimal characters.</p>
            <div><Button type="button" variant="text" disabled={busy} onclick={clearNewArtifact}>Discard new artifact</Button></div>
          </div>
        {:else}
          <div><Button type="button" variant="text" disabled={busy} onclick={() => { addingNewArtifact = true; resetAttempt(); }}>Add new packaged artifact</Button></div>
        {/if}
        <div><Button type="submit" disabled={busy || (!advanced && !fieldsValid)}>Create proposal</Button></div>
      {/if}
    </form>
  </section>

  {#if selected}
    <section class="section">
      <h2>Change {selected.id}</h2>
      <p class="muted">{selected.service} · base <code>{selected.base_commit}</code> · {selected.state}</p>
      <p><a href={`/changes?${new URLSearchParams({ change: rolloutId || selected.id })}`}>Shareable rollout link</a></p>
      <p class="muted">Creating, refreshing, validating, and publishing proposals require a platform administrator. Service-owner rollout permissions are shown in the rollout panel.</p>
      {#if actionError}<div class="error" role="alert">{actionError}</div>{/if}
      <div class="actions">
        <Button variant="outlined" onclick={refreshChange} disabled={busy || !['proposed', 'validated', 'open'].includes(selected.state)}>Refresh proposal</Button>
        <Button onclick={validate} disabled={busy || !['proposed', 'validated'].includes(selected.state)}>Validate and render</Button>
        {#if validation && !validation.errors.length && !prUrl}<Button variant="outlined" onclick={openPr} disabled={busy}>Open or update pull request</Button>{/if}
      </div>
      {#if selected.state === 'open'}<p class="muted">If the base moved, refresh rebases the preview and clears prior rollout approvals. Validate and review the new diffs, then update the pull request; renewed owner approval, forge reviews, and checks are required.</p>
      {:else if !['proposed', 'validated'].includes(selected.state)}<p class="muted">This change is no longer awaiting proposal validation or refresh.</p>{/if}
      {#if refreshed && !validation}<p role="status">Proposal refreshed. Validate and review the regenerated diffs before opening or updating the pull request.</p>{/if}
      {#if validation}
        {#if validation.errors.length}<div class="error" role="alert"><strong>Validation failed</strong><ul>{#each validation.errors as issue}<li>{issue}</li>{/each}</ul></div>
        {:else}<p class="notice">Validation passed. Review both diffs before opening a pull request.</p>{/if}
        <h3>Data diff</h3><pre>{validation.data_diff || 'No data changes returned.'}</pre>
        <h3>Rendered manifest diff</h3><pre>{validation.rendered_diff || 'No rendered changes returned.'}</pre>
      {/if}
      {#if prUrl}<p role="status">Pull request available: <a href={prUrl} target="_blank" rel="noopener noreferrer">{prUrl}</a></p>{/if}
    </section>
  {/if}
</div>
