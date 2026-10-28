<script lang="ts">
  import { onMount } from 'svelte';
  import { Button } from 'm3-svelte';
  import { ApiError, errorMessage } from './api';
  import { equalJson } from './ChangeForm';
  import { approveChangeRollout, getChangeRollout, scheduleChangeRollout, type ChangeApprovalBinding, type ChangeRolloutView } from './ChangeApi';

  export let changeId: string;
  export let onSelectChange: (id: string) => void;
  export let canSelectChange: (id: string) => boolean;
  export let selectionDisabled = false;

  let view: ChangeRolloutView | undefined;
  let loading = true;
  let busy = false;
  let stale = false;
  let error = '';
  let actionError = '';
  let observedAt: Date | undefined;
  let windowStart = '';
  let windowEnd = '';
  let windowEdited = false;
  let approvalReview: ChangeApprovalBinding | undefined;
  let approvalReviewStale = false;
  $: approvalReviewStale = !!approvalReview && (!view || !view.can_approve || !equalJson(view.approval_binding, approvalReview));
  let timer: ReturnType<typeof setTimeout> | undefined;
  let controller: AbortController | undefined;
  let sequence = 0;
  let alive = true;

  function timestamp(value: string | null | undefined): string {
    if (!value) return 'Unknown';
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? 'Unknown' : `${date.toLocaleString()} (${date.toISOString()})`;
  }
  function windowPosition(value: string | null | undefined, windowEnd: string | null | undefined): 'after' | 'at_or_before' | 'unknown' {
    if (!value || !windowEnd) return 'unknown';
    const time = Date.parse(value);
    const end = Date.parse(windowEnd);
    if (!Number.isFinite(time) || !Number.isFinite(end)) return 'unknown';
    return time > end ? 'after' : 'at_or_before';
  }
  function utcInput(value: string | null | undefined): string {
    if (!value) return '';
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? '' : date.toISOString().slice(0, 19);
  }
  function update(next: ChangeRolloutView) {
    view = next;
    stale = false;
    error = '';
    observedAt = new Date();
    if (!windowEdited) {
      windowStart = utcInput(next.status.window_start);
      windowEnd = utcInput(next.status.window_end);
    }
  }
  function scheduleRefresh() {
    if (timer) clearTimeout(timer);
    if (alive) timer = setTimeout(() => void refresh(), 15000);
  }
  async function refresh() {
    if (!alive || busy) return;
    if (timer) clearTimeout(timer);
    controller?.abort();
    const localController = new AbortController();
    controller = localController;
    const request = ++sequence;
    loading = !view;
    let timedOut = false;
    const deadline = setTimeout(() => { timedOut = true; localController.abort(); }, 15000);
    try {
      const next = await getChangeRollout(view?.status.change_id ?? changeId, localController.signal);
      if (alive && request === sequence) update(next);
    } catch (failure) {
      if (alive && request === sequence && (!localController.signal.aborted || timedOut)) {
        stale = !!view;
        error = timedOut ? 'The rollout request timed out.' : errorMessage(failure);
      }
    } finally {
      clearTimeout(deadline);
      if (alive && request === sequence) {
        loading = false;
        scheduleRefresh();
      }
    }
  }
  async function schedule() {
    if (!view || !view.can_schedule || stale || busy) return;
    actionError = '';
    approvalReview = undefined;
    const start = new Date(`${windowStart}Z`);
    const end = new Date(`${windowEnd}Z`);
    if (!windowStart || !windowEnd || Number.isNaN(start.getTime()) || Number.isNaN(end.getTime()) || end.getTime() <= start.getTime()) {
      actionError = 'Enter a valid UTC start and a later UTC end for the window.';
      return;
    }
    const request = ++sequence;
    controller?.abort();
    const localController = new AbortController();
    controller = localController;
    if (timer) clearTimeout(timer);
    busy = true;
    let timedOut = false;
    const deadline = setTimeout(() => { timedOut = true; localController.abort(); }, 30000);
    try {
      const next = await scheduleChangeRollout(view.status.change_id, { window_start: start.toISOString(), window_end: end.toISOString() }, localController.signal);
      if (alive && request === sequence) {
        windowEdited = false;
        update(next);
      }
    } catch (failure) {
      if (alive && request === sequence) {
        stale = true;
        actionError = `${timedOut ? 'The scheduling request timed out.' : errorMessage(failure)} Refresh the rollout before retrying; the request may have been processed.`;
      }
    } finally {
      clearTimeout(deadline);
      if (alive && request === sequence) {
        busy = false;
        void refresh();
      }
    }
  }
  function reviewApproval() {
    if (!view || !view.can_approve || !view.approval_binding || stale || busy) return;
    const end = Date.parse(view.approval_binding.window_end);
    if (!Number.isFinite(end) || end <= Date.now()) {
      actionError = 'The scheduled window is unknown or has expired. Refresh the rollout before approving.';
      void refresh();
      return;
    }
    approvalReview = structuredClone(view.approval_binding);
    actionError = '';
  }

  async function approve() {
    if (!view || !view.can_approve || !approvalReview || stale || busy || !equalJson(view.approval_binding, approvalReview)) return;
    const binding = structuredClone(approvalReview);
    const end = Date.parse(binding.window_end);
    if (!Number.isFinite(end) || end <= Date.now()) {
      actionError = 'The scheduled window is unknown or has expired. Refresh the rollout before approving.';
      void refresh();
      return;
    }
    actionError = '';
    const request = ++sequence;
    controller?.abort();
    const localController = new AbortController();
    controller = localController;
    if (timer) clearTimeout(timer);
    busy = true;
    let timedOut = false;
    const deadline = setTimeout(() => { timedOut = true; localController.abort(); }, 30000);
    try {
      const next = await approveChangeRollout(view.status.change_id, binding, localController.signal);
      if (alive && request === sequence) {
        approvalReview = undefined;
        update(next);
      }
    } catch (failure) {
      if (alive && request === sequence) {
        stale = true;
        approvalReview = undefined;
        actionError = failure instanceof ApiError && failure.status === 409
          ? 'The approval snapshot changed. Refresh and review the current PR and window before approving.'
          : `${timedOut ? 'The approval request timed out.' : errorMessage(failure)} Refresh the rollout before retrying; the request may have been processed.`;
      }
    } finally {
      clearTimeout(deadline);
      if (alive && request === sequence) {
        busy = false;
        void refresh();
      }
    }
  }

  onMount(() => {
    void refresh();
    return () => {
      alive = false;
      sequence += 1;
      controller?.abort();
      if (timer) clearTimeout(timer);
    };
  });
</script>

<section class="section" aria-labelledby="rollout-heading">
  <div class="heading"><div><h3 id="rollout-heading">Rollout and promotion</h3><p class="muted">Rollout status is read from the control plane; review and merge remain governed by the forge.</p></div><Button type="button" variant="outlined" disabled={busy} onclick={refresh}>Refresh rollout</Button></div>
  {#if loading}<p role="status">Loading rollout…</p>{/if}
  {#if error}<div class="error" role="alert">{stale ? 'Stale rollout: the latest refresh failed. Actions are disabled.' : 'Rollout status is Unknown.'} {error}</div>{/if}
  {#if actionError}<div class="error" role="alert">{actionError}</div>{/if}
  {#if view}
    <div class="panel stack">
      <p><strong>Rollout change:</strong> <code>{view.status.change_id}</code>{#if view.status.change_id !== changeId} (resolved from change <code>{changeId}</code>){/if}</p>
      <p><strong>Mode:</strong> {view.mode || 'Unknown'} · <strong>Stage:</strong> {view.status.stage || 'Unknown'}{#if stale} · <strong>Stale</strong>{/if}</p>
      {#if observedAt}<p class="muted">Last refreshed {observedAt.toLocaleString()}.</p>{/if}
      {#if view.mode === 'gated'}
        <p class="muted">The worker follows the candidate PR and validation, then the promotion PR, public validation, and cleanup. Promotion occurs only after the required evidence gates; PR review and merge still apply.</p>
      {:else if view.mode === 'in_place' || view.mode === 'in-place'}
        <p class="muted">An owner-team administrator schedules and approves the window. Approval is bound to that window and the current PR head; rescheduling invalidates earlier approvals.</p>
        <p class="muted">The scheduled window governs merge initiation, not guaranteed completion. The merge or Flux reconciliation can finish after the window; controller observation time is neither the forge's merge time nor Flux completion time.</p>
      {:else if view.mode === 'rolling'}
        <p class="muted">The worker follows the reviewed rolling change and its validation.</p>
      {/if}
      {#if view.status.error}<div class="error" role="alert">{view.status.error}</div>{/if}
      <div class="grid">
        <div><strong>Window start</strong><p>{timestamp(view.status.window_start)}</p></div>
        <div><strong>Window end</strong><p>{timestamp(view.status.window_end)}</p></div>
        <div><strong>Candidate deployment</strong><p>{view.status.candidate_deployment || 'Unknown'}</p></div>
        <div><strong>Candidate revision</strong><p><code>{view.status.candidate_revision || 'Unknown'}</code></p></div>
        <div><strong>Previous deployment</strong><p>{view.status.old_deployment || 'Unknown'}</p></div>
        <div><strong>Candidate validation run</strong><p><code>{view.status.candidate_validation_run || 'Unknown'}</code></p></div>
        <div><strong>Public validation run</strong><p><code>{view.status.public_validation_run || 'Unknown'}</code></p></div>
        <div><strong>Merge requested locally</strong><p>{timestamp(view.status.merge_requested_at)}</p></div>
        <div><strong>Merge observed by controller</strong><p>{timestamp(view.status.merge_observed_at)}{#if view.status.window_end}{#if windowPosition(view.status.merge_observed_at, view.status.window_end) === 'after'} · Observed after window end{:else if windowPosition(view.status.merge_observed_at, view.status.window_end) === 'at_or_before'} · Observed at or before window end{:else} · Window comparison Unknown{/if}{/if}</p></div>
        <div><strong>Merge time reported by GitHub</strong><p>{timestamp(view.status.upstream_merged_at)}{#if view.status.window_end}{#if windowPosition(view.status.upstream_merged_at, view.status.window_end) === 'after'} · GitHub reported merge after window end{:else if windowPosition(view.status.upstream_merged_at, view.status.window_end) === 'at_or_before'} · GitHub reported merge at or before window end{:else} · Window comparison Unknown{/if}{/if}</p></div>
      </div>
      {#if view.status.in_place_pool}
        <div>
          <h3>Shared-pool in-place evidence</h3>
          <p>Previous revision: <code>{view.status.previous_revision || 'Unknown'}</code><br />Workload namespace: {view.status.workload_namespace || 'Unknown'}<br />Filler evidence observed: {timestamp(view.status.filler_verified_at)}</p>
          <p class="muted">This is point-in-time evidence. It can expire and is not an atomic merge guarantee.</p>
        </div>
      {/if}
      {#if view.status.queued}<p role="status">Rollout reconciliation is queued.</p>{/if}
      {#if view.status.stages.length}
        <div><h3>Recorded stages</h3><div class="table-wrap"><table><thead><tr><th scope="col">Stage</th><th scope="col">Change</th><th scope="col">Merged commit</th><th scope="col">Validation run</th></tr></thead><tbody>
          {#each view.status.stages as stage}<tr><td>{stage.stage}</td><td>{#if stage.change_id !== view.status.change_id && canSelectChange(stage.change_id)}<Button type="button" variant="text" disabled={selectionDisabled} onclick={() => onSelectChange(stage.change_id)}>{stage.change_id}</Button>{:else}{stage.change_id || 'Unknown'}{/if}</td><td><code>{stage.merged_commit || 'Unknown'}</code></td><td><code>{stage.validation_run || 'Unknown'}</code></td></tr>{/each}
        </tbody></table></div></div>
      {/if}
      <div><h3>Approvals</h3>
        {#if view.status.approvals.length}
          <div class="table-wrap"><table><thead><tr><th scope="col">Actor</th><th scope="col">Owner team</th><th scope="col">PR head</th><th scope="col">Base</th><th scope="col">Window (UTC)</th><th scope="col">Approved</th></tr></thead><tbody>
            {#each view.status.approvals as approval}<tr><td>{approval.actor}</td><td>{approval.owner_team}</td><td><code>{approval.head_sha}</code></td><td><code>{approval.base_sha}</code></td><td>{approval.window_start} – {approval.window_end}</td><td>{timestamp(approval.approved_at)}</td></tr>{/each}
          </tbody></table></div>
        {:else}<p class="muted">No approvals recorded.</p>{/if}
      </div>
      {#if view.can_schedule}
        <form class="stack" onsubmit={(event) => { event.preventDefault(); void schedule(); }}>
          <h3>Schedule an in-place window</h3>
          <label>Window start (UTC)<input type="datetime-local" step="1" required disabled={busy || stale} bind:value={windowStart} oninput={() => { windowEdited = true; }} /></label>
          <label>Window end (UTC)<input type="datetime-local" step="1" required disabled={busy || stale} bind:value={windowEnd} oninput={() => { windowEdited = true; }} /></label>
          <p class="muted">Saving a new window invalidates any approval of the previous window.</p>
          <div><Button type="submit" disabled={busy || stale}>Save window</Button></div>
        </form>
      {/if}
      {#if view.can_approve && view.approval_binding}
        <div>
          <h3>Current approval binding</h3>
          <p>PR head: <code>{view.approval_binding.head_sha}</code><br />PR base: <code>{view.approval_binding.base_sha}</code><br />Window (UTC): {view.approval_binding.window_start} – {view.approval_binding.window_end}<br />Policy digest: <code>{view.approval_binding.policy_digest}</code></p>
          {#if !approvalReview || approvalReviewStale}<Button type="button" disabled={busy || stale} onclick={reviewApproval}>Review approval</Button>{/if}
        </div>
      {:else if view.can_approve}
        <p class="error">The approval snapshot is unavailable. Refresh the rollout before approving.</p>
      {/if}
      {#if approvalReview}
        <div class="panel">
          <h3>Confirm the reviewed approval</h3>
          <p>PR head: <code>{approvalReview.head_sha}</code><br />PR base: <code>{approvalReview.base_sha}</code><br />Window (UTC): {approvalReview.window_start} – {approvalReview.window_end}<br />Policy digest: <code>{approvalReview.policy_digest}</code></p>
          <p class="muted">Approval binds exactly this PR, base, window and policy. A changed snapshot cannot be approved without a new review.</p>
          {#if approvalReviewStale}<p class="error" role="alert">This approval snapshot changed. Review the current binding again.</p>{/if}
          <div class="actions">
            <Button type="button" disabled={busy || stale || approvalReviewStale} onclick={approve}>Confirm approval</Button>
            <Button type="button" variant="text" disabled={busy} onclick={() => { approvalReview = undefined; }}>Cancel</Button>
          </div>
        </div>
      {/if}
      {#if !view.can_schedule && !view.can_approve && (view.mode === 'in_place' || view.mode === 'in-place')}
        <p class="muted">Scheduling and approval are not currently available for this account or rollout stage.</p>
      {/if}
    </div>
  {/if}
</section>
