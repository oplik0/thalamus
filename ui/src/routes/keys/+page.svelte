<script lang="ts">
  import { onMount } from 'svelte'; import { Button } from 'm3-svelte';
  import { api, errorMessage, type Key, type Project, type Service, type Team } from '$lib/api';
  let keys: Key[] = []; let projects: Project[] = []; let teams: Team[] = []; let services: Service[] = []; let loading = true; let loaded = false; let busy = false; let error = ''; let secret = ''; let copyMessage = '';
  let name = ''; let kind: Parameters<typeof api.createKey>[0]['kind'] = 'project'; let project = ''; let teamId = ''; let allowed: string[] = []; let expires = ''; let alive = true;
  async function load() {
    loading = true; loaded = false; error = '';
    try {
      const [k, p, t, s] = await Promise.all([api.keys(), api.projects(), api.teams(), api.services()]);
      if (!alive) return;
      keys = k.keys; projects = p.projects; teams = t.teams; services = s.services;
      if (!projects.some(item => item.id === project)) project = '';
      if (!teams.some(item => item.id === teamId)) teamId = '';
      loaded = true;
    } catch (e) { if (alive) error = errorMessage(e); } finally { if (alive) loading = false; }
  }
  async function create() {
    busy = true; error = ''; secret = ''; copyMessage = '';
    try {
      const result = await api.createKey({
        name, kind, team_id: kind === 'personal' ? teamId : null,
        project_id: kind === 'project' ? project : null,
        allowed_services: allowed, expires_at: expires ? new Date(expires).toISOString() : null
      });
      if (!alive) return;
      secret = result.secret; name = ''; expires = ''; allowed = []; loaded = false;
      const refreshed = await api.keys();
      if (alive) { keys = refreshed.keys; loaded = true; }
    } catch (e) { if (alive) error = errorMessage(e); } finally { if (alive) busy = false; }
  }
  async function revoke(key: Key) { if (!confirm(`Revoke ${key.name}? Access may remain until the gateway picks up the change.`)) return; busy = true; error = ''; try { await api.revokeKey(key.id); loaded = false; keys = (await api.keys()).keys; loaded = true; } catch (e) { error = errorMessage(e); } finally { busy = false; } }
  async function verifyRevocation(key: Key, form: HTMLFormElement) {
    const value = new FormData(form).get('key');
    form.reset();
    if (typeof value !== 'string' || !value) return;
    busy = true; error = '';
    try {
      await api.verifyKeyRevocation(key.id, value);
      keys = (await api.keys()).keys;
    } catch (e) {
      error = errorMessage(e).split(value).join('[redacted]');
    } finally { busy = false; }
  }
  async function copy() { try { await navigator.clipboard.writeText(secret); copyMessage = 'Copied to clipboard.'; } catch { copyMessage = 'Copy failed. Select and copy the secret manually.'; } }
  onMount(() => { void load(); return () => { alive = false; secret = ''; copyMessage = ''; }; });
</script>
<div class="page"><div class="heading"><div><h1>API keys</h1><p class="subtitle">Issue and revoke personal or project access. Gateway projection may take time to become effective.</p></div><Button variant="outlined" onclick={load}>Refresh</Button></div>
{#if error}<div class="error" role="alert">{error}</div>{/if}{#if secret}<div class="notice" role="status"><strong>Save this key now.</strong> It cannot be retrieved again.<pre class="secret">{secret}</pre><div class="actions"><Button variant="outlined" onclick={copy}>Copy key</Button><Button variant="text" onclick={() => { secret = ''; copyMessage = ''; }}>Dismiss</Button><span>{copyMessage}</span></div></div>{/if}
<section>
  <h2>Your keys</h2>
  {#if loading}<p role="status">Loading keys…</p>
  {:else if loaded && !keys.length}<p class="muted">No keys returned.</p>
  {:else if loaded}
    <div class="table-wrap"><table><thead><tr><th>Name</th><th>Kind</th><th>Team</th><th>Project</th><th>Allowed services</th><th>Expires</th><th>State</th><th>Action</th></tr></thead><tbody>
      {#each keys as key}
        <tr>
          <td>{key.name}</td>
          <td>{key.kind}</td>
          <td>{teams.find(item => item.id === key.team_id)?.name ?? key.team_id}</td>
          <td>{key.project_id ? (projects.find(item => item.id === key.project_id)?.name ?? key.project_id) : 'Not associated'}</td>
          <td>{key.allowed_services.join(', ') || 'None'}</td>
          <td>{key.expires_at ? new Date(key.expires_at).toLocaleString() : 'No expiry'}</td>
          <td>{key.revocation_state}</td>
          <td>
            {#if key.revocation_state === 'revocation pending'}
              <form class="stack" onsubmit={(event) => { event.preventDefault(); verifyRevocation(key, event.currentTarget); }}><label>Paste key to verify revocation<input type="password" name="key" autocomplete="off" required /></label><Button type="submit" disabled={busy}>Verify revocation</Button></form>
            {:else if key.revocation_state !== 'revocation verified'}
              <Button variant="text" disabled={busy} onclick={() => revoke(key)}>Revoke</Button>
            {/if}
          </td>
        </tr>
      {/each}
    </tbody></table></div>
  {/if}
</section>
<section class="section">
  <h2>Create a key</h2>
  <form class="panel form stack" onsubmit={(event) => { event.preventDefault(); create(); }}>
    <label>Name<input required bind:value={name} /></label>
    <label>Key kind
      <select bind:value={kind} onchange={() => { project = ''; teamId = ''; }}>
        <option value="project">Project</option>
        <option value="personal">Personal</option>
      </select>
    </label>
    {#if kind === 'project'}
      <label>Project<select required bind:value={project}><option value="" disabled>Select a project</option>{#each projects as item}<option value={item.id}>{item.name} · {item.team}</option>{/each}</select></label>
      {#if loaded && !projects.length}<p class="muted">No projects are available. Create a project before issuing a project key.</p>{/if}
    {:else}
      <label>Team<select required bind:value={teamId}><option value="" disabled>Select a team</option>{#each teams as item}<option value={item.id}>{item.name}</option>{/each}</select></label>
      {#if loaded && !teams.length}<p class="muted">No teams are available for a personal key.</p>{/if}
    {/if}
    <label>Allowed services<select multiple bind:value={allowed} required>{#each services as item}<option value={item.name}>{item.name}</option>{/each}</select></label>
    <span class="muted">Select one or more services using Ctrl or Command. The server checks each selection against the selected team's committed policy; some services visible to you may not be allowed for that team.</span>
    <label>Expires at (optional)<input type="datetime-local" bind:value={expires} /></label>
    <div><Button disabled={busy || loading || !loaded} type="submit">Create key</Button></div>
  </form>
</section></div>
