<script lang="ts">
  import { Button } from 'm3-svelte';
  import { deploymentFields, equalJson, isChangeObject, setChangeValue, type ChangeField, type ChangeObject } from './ChangeForm';
  import type { ChangeServiceForm } from './ChangeApi';

  export let schema: unknown;
  export let draft: ChangeObject;
  export let base: ChangeObject;
  export let artifacts: ChangeServiceForm['artifacts'];
  export let disabled = false;
  export let deploymentIndex: number;
  export let onChange: (value: ChangeObject) => void;
  export let valid = true;

  let fields: ChangeField[] = [];
  let schemaError = '';
  let inputs: Record<string, string> = {};
  let errors: Record<string, string> = {};
  let artifactOptions: { name: string; uri: string }[] = [];
  let currentDeployment: unknown;
  let packaged = false;
  let imageValue = '';
  let matchingArtifacts: { name: string; uri: string }[] = [];

  $: artifactOptions = Object.entries(artifacts).flatMap(([name, value]) =>
    isChangeObject(value) && typeof value.uri === 'string' ? [{ name, uri: value.uri }] : []);
  $: currentDeployment = Array.isArray(draft.deployments) ? draft.deployments[deploymentIndex] : undefined;
  $: packaged = isChangeObject(currentDeployment) && currentDeployment.packagedModel === true;
  $: imageValue = isChangeObject(currentDeployment) && typeof currentDeployment.image === 'string' ? currentDeployment.image : '';
  $: matchingArtifacts = imageValue ? artifactOptions.filter(artifact => artifact.uri === `oci://${imageValue}`) : [];

  $: {
    try {
      fields = deploymentFields(schema, draft, deploymentIndex);
      schemaError = '';
    } catch (error) {
      fields = [];
      schemaError = error instanceof Error ? error.message : 'The deployment schema is unavailable.';
    }
  }
  $: valid = !schemaError && Object.keys(errors).length === 0;

  const keyFor = (field: ChangeField) => JSON.stringify(field.path);
  function setError(key: string, message: string) {
    if (message) errors = { ...errors, [key]: message };
    else {
      const next = { ...errors };
      delete next[key];
      errors = next;
    }
  }
  function update(field: ChangeField, value: unknown, remove = false) {
    const key = keyFor(field);
    try {
      onChange(setChangeValue(draft, field.path, value, remove, base));
      setError(key, '');
    } catch (error) {
      setError(key, error instanceof Error ? error.message : 'Could not update this field.');
    }
  }
  function edit(field: ChangeField, raw: string) {
    const key = keyFor(field);
    inputs = { ...inputs, [key]: raw };
    if (field.kind === 'string') {
      update(field, raw);
    } else if (field.kind === 'integer' || field.kind === 'number') {
      const value = Number(raw);
      if (!raw.trim() || !Number.isFinite(value) || (field.kind === 'integer' && !Number.isInteger(value))) {
        setError(key, field.kind === 'integer' ? 'Enter a whole number, or unset this optional field.' : 'Enter a number, or unset this optional field.');
      } else if (Number.isInteger(value) && !Number.isSafeInteger(value)) {
        setError(key, 'This integer is outside the range that the browser can preserve exactly.');
      } else {
        update(field, value);
      }
    } else {
      try {
        update(field, JSON.parse(raw));
      } catch {
        setError(key, 'Enter valid JSON.');
      }
    }
  }
  function unset(field: ChangeField) {
    const key = keyFor(field);
    const next = { ...inputs };
    delete next[key];
    inputs = next;
    update(field, undefined, true);
  }
  function rawValue(field: ChangeField): string {
    const key = keyFor(field);
    if (Object.hasOwn(inputs, key)) return inputs[key];
    if (!field.present) return '';
    if (field.kind === 'json') return JSON.stringify(field.value, null, 2) ?? '';
    return String(field.value ?? '');
  }
  function selectedOption(field: ChangeField): string {
    if (!field.present || !field.options) return '';
    const index = field.options.findIndex(option => equalJson(option, field.value));
    return index < 0 ? '' : String(index);
  }
</script>

{#if schemaError}
  <div class="error" role="alert">Cannot build the deployment form: {schemaError}</div>
{:else}
  <div class="stack">
    {#each fields as field (keyFor(field))}
      {@const key = keyFor(field)}
      {@const workflowRevision = field.path.length === 3 && field.path[2] === 'revision'}
      {@const artifactField = field.path.length === 3 && field.path[2] === 'artifact'}
      <div>
        <label>
          {field.label}{#if workflowRevision} <span class="muted">(workflow-managed)</span>{:else if field.required} <span>(required)</span>{:else if !field.present} <span class="muted">(not set in catalog)</span>{/if}
          {#if workflowRevision}
            <input type="text" readonly value={typeof field.value === 'string' ? field.value : ''} placeholder="Not available in the committed catalog" />
          {:else if artifactField}
            <select value={typeof field.value === 'string' ? field.value : ''} required={field.required && !field.present} {disabled}
              onchange={(event) => { if (event.currentTarget.value) update(field, event.currentTarget.value); }}>
              <option value="" disabled>Select a committed artifact</option>
              {#if typeof field.value === 'string' && field.value && !artifactOptions.some(artifact => artifact.name === field.value)}
                <option value={field.value}>{field.value} (not found in the committed artifact catalog)</option>
              {/if}
              {#each artifactOptions as artifact (artifact.name)}
                <option value={artifact.name}>{artifact.name} — {artifact.uri}</option>
              {/each}
            </select>
          {:else if field.options}
            <select value={selectedOption(field)} required={field.required && !field.present} {disabled} aria-invalid={!!errors[key]}
              onchange={(event) => {
                const index = event.currentTarget.value;
                if (index !== '') update(field, field.options?.[Number(index)]);
              }}>
              <option value="" disabled>{field.present ? 'Current value is not listed in the schema' : 'Not set'}</option>
              {#each field.options as option, index}<option value={String(index)}>{JSON.stringify(option)}</option>{/each}
            </select>
          {:else if field.kind === 'boolean'}
            <select value={field.present && typeof field.value === 'boolean' ? String(field.value) : ''} required={field.required && !field.present} {disabled} aria-invalid={!!errors[key]}
              onchange={(event) => { if (event.currentTarget.value) update(field, event.currentTarget.value === 'true'); }}>
              <option value="" disabled>{field.present ? 'Current value does not match the schema' : 'Not set'}</option>
              <option value="true">True</option><option value="false">False</option>
            </select>
          {:else if field.kind === 'json'}
            <textarea spellcheck="false" value={rawValue(field)} required={field.required && !field.present} {disabled} aria-invalid={!!errors[key]}
              oninput={(event) => edit(field, event.currentTarget.value)}></textarea>
          {:else}
            <input type={field.kind === 'string' ? 'text' : 'number'} value={rawValue(field)} required={field.required && !field.present} {disabled}
              min={field.minimum} max={field.maximum} minlength={field.minLength} maxlength={field.maxLength}
              step={field.kind === 'integer' ? '1' : field.kind === 'number' ? 'any' : undefined}
              aria-invalid={!!errors[key]} oninput={(event) => edit(field, event.currentTarget.value)} />
          {/if}
        </label>
        {#if field.description}<p class="muted">{field.description}</p>{/if}
        {#if workflowRevision}<p class="muted">The workflow derives the deployment revision from its configuration and dependencies during validation.</p>{/if}
        {#if artifactField && packaged && imageValue}
          {#if !matchingArtifacts.length}
            <p class="error">No committed artifact has the URI <code>oci://{imageValue}</code>. Choose a matching artifact or add a new packaged artifact to this proposal below.</p>
          {:else if !matchingArtifacts.some(artifact => artifact.name === field.value)}
            <p class="muted">The packaged image requires an artifact with URI <code>oci://{imageValue}</code>.</p>
            {#each matchingArtifacts as artifact (artifact.name)}<Button type="button" variant="text" {disabled} onclick={() => update(field, artifact.name)}>Use {artifact.name}</Button>{/each}
          {:else}
            <p class="muted">The selected committed artifact matches this packaged image.</p>
          {/if}
        {/if}
        {#if field.kind === 'json'}<p class="muted">This schema value is edited as JSON; unspecified sibling values are preserved.</p>{/if}
        {#if errors[key]}<p class="error" role="alert">{errors[key]}</p>{/if}
        {#if !workflowRevision && !field.required && field.present}<Button type="button" variant="text" {disabled} onclick={() => unset(field)}>Unset {field.label}</Button>{/if}
      </div>
    {/each}
  </div>
{/if}
