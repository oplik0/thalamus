<script lang="ts">
  import { onMount } from 'svelte';
  import { Button } from 'm3-svelte';
  import { errorMessage, type Run } from './api';
  import { benchmarkApi, type BenchmarkComparison, type BenchmarkResult } from './BenchmarkApi';
  import { benchmarkChartSeries, benchmarkNumber } from './BenchmarkChartData';
  import BenchmarkChart from './BenchmarkChart.svelte';

  export let run: Run;
  export let runs: Run[];

  let result: BenchmarkResult | undefined;
  let comparison: BenchmarkComparison | undefined;
  let candidateId = '';
  let loading = true;
  let comparing = false;
  let error = '';
  let comparisonError = '';
  let mounted = false;
  let resultVersion = 0;
  let comparisonVersion = 0;
  let resultController: AbortController | undefined;
  let comparisonController: AbortController | undefined;

  $: candidates = runs.filter(item => item.id !== run.id && item.kind === 'benchmark' && item.state === 'succeeded' && item.service === run.service);
  $: views = comparison
    ? [{ label: 'Baseline', result: comparison.baseline }, { label: 'Comparison', result: comparison.candidate }]
    : result ? [{ label: 'Selected run', result }] : [];
  $: ttftSeries = benchmarkChartSeries(views, 'ttft');
  $: itlSeries = benchmarkChartSeries(views, 'itl');
  $: requestSeries = benchmarkChartSeries(views, 'request-rates');
  $: tokenSeries = benchmarkChartSeries(views, 'output-tokens');
  $: differenceRows = comparison ? [
    { label: 'Deployment', baseline: comparison.deployment.baseline, candidate: comparison.deployment.candidate, changed: comparison.deployment.changed },
    { label: 'Desired configuration', baseline: comparison.desired_configuration.baseline, candidate: comparison.desired_configuration.candidate, changed: comparison.desired_configuration.changed },
    { label: 'Observed runtime identity', baseline: comparison.observed_runtime.baseline, candidate: comparison.observed_runtime.candidate, changed: comparison.observed_runtime.changed },
    { label: 'Hardware', baseline: comparison.conditions.baseline.hardware, candidate: comparison.conditions.candidate.hardware, changed: comparison.conditions.baseline.hardware !== comparison.conditions.candidate.hardware },
    { label: 'Nodes', baseline: comparison.conditions.baseline.nodes.join(', '), candidate: comparison.conditions.candidate.nodes.join(', '), changed: JSON.stringify(comparison.conditions.baseline.nodes) !== JSON.stringify(comparison.conditions.candidate.nodes) },
    { label: 'Replicas', baseline: String(comparison.conditions.baseline.replicas), candidate: String(comparison.conditions.candidate.replicas), changed: comparison.conditions.baseline.replicas !== comparison.conditions.candidate.replicas },
    { label: 'Cache state', baseline: comparison.conditions.baseline.cache_state, candidate: comparison.conditions.candidate.cache_state, changed: comparison.conditions.baseline.cache_state !== comparison.conditions.candidate.cache_state },
    { label: 'Competing traffic', baseline: comparison.conditions.baseline.competing_traffic, candidate: comparison.conditions.candidate.competing_traffic, changed: comparison.conditions.baseline.competing_traffic !== comparison.conditions.candidate.competing_traffic }
  ] : [];

  async function loadResult() {
    const version = ++resultVersion;
    resultController?.abort();
    const controller = new AbortController();
    resultController = controller;
    loading = true;
    error = '';
    try {
      const value = await benchmarkApi.result(run.id, controller.signal);
      if (mounted && version === resultVersion) result = value;
    } catch (e) {
      if (mounted && version === resultVersion) {
        result = undefined;
        error = errorMessage(e);
      }
    } finally {
      if (mounted && version === resultVersion) loading = false;
      if (resultController === controller) resultController = undefined;
    }
  }

  function clearComparison() {
    comparisonVersion++;
    comparisonController?.abort();
    comparisonController = undefined;
    comparing = false;
    comparison = undefined;
    comparisonError = '';
  }

  async function compare() {
    if (!candidateId) return;
    const version = ++comparisonVersion;
    comparisonController?.abort();
    const controller = new AbortController();
    comparisonController = controller;
    const candidate = candidateId;
    comparing = true;
    comparison = undefined;
    comparisonError = '';
    try {
      const value = await benchmarkApi.compare(run.id, candidate, controller.signal);
      if (mounted && version === comparisonVersion) comparison = value;
    } catch (e) {
      if (mounted && version === comparisonVersion) comparisonError = errorMessage(e);
    } finally {
      if (mounted && version === comparisonVersion) comparing = false;
      if (comparisonController === controller) comparisonController = undefined;
    }
  }

  onMount(() => {
    mounted = true;
    void loadResult();
    return () => {
      mounted = false;
      resultVersion++;
      comparisonVersion++;
      resultController?.abort();
      comparisonController?.abort();
    };
  });
</script>

<section class="section">
  <h2>Benchmark evidence</h2>
  {#if loading}
    <p role="status">Loading benchmark evidence…</p>
  {:else if error}
    <div class="error" role="alert">Could not load benchmark evidence: {error}</div>
    <Button variant="outlined" onclick={loadResult}>Retry evidence</Button>
  {:else if result}
    <div class="panel compare-form">
      <h3>Compare runs</h3>
      <p class="muted">Choose another completed benchmark for this service. The server verifies the project, workload, test, runner and tokenizer identities before comparing; incompatible runs cannot be compared.</p>
      {#if candidates.length}
        <div class="actions">
          <label>Comparison run
            <select bind:value={candidateId} onchange={clearComparison}>
              <option value="">Select a run</option>
              {#each candidates as candidate (candidate.id)}
                <option value={candidate.id}>{candidate.id} · {candidate.deployment} · configuration {candidate.revision}</option>
              {/each}
            </select>
          </label>
          <Button disabled={!candidateId || comparing} onclick={compare}>{comparing ? 'Comparing…' : 'Compare'}</Button>
        </div>
      {:else}
        <p class="muted">No other completed benchmark for this service was returned.</p>
      {/if}
      {#if comparisonError}<div class="error" role="alert">Could not compare these runs: {comparisonError}</div>{/if}
    </div>

    {#if comparison}
      <section class="section">
        <h3>Differing conditions and identities</h3>
        <p class="muted">The selected run is the baseline. Observation times are provenance, not condition differences.</p>
        {#if !differenceRows.some(row => row.changed)}<p>No differences were reported in these identities or conditions.</p>{/if}
        <div class="table-wrap">
          <table>
            <thead><tr><th>Field</th><th>Baseline</th><th>Comparison</th><th>Difference</th></tr></thead>
            <tbody>
              {#each differenceRows as row}
                <tr><td>{row.label}</td><td>{row.baseline}</td><td>{row.candidate}</td><td>{row.changed ? 'Different' : 'Same'}</td></tr>
              {/each}
            </tbody>
          </table>
        </div>
      </section>
    {/if}

    <section class="section">
      <h3>Measured curves</h3>
      <p class="muted">The horizontal axis is the observed request-start rate, not the configured target, concurrency, or successful throughput. It counts recorded successes and failures over the first-to-last start-time window; dropped or unrecorded requests are excluded, and rates from few requests can be biased. Stages with an unknown start rate remain in the table but cannot be plotted. A missing value breaks the line.</p>
      <p class="muted">TTFT percentiles are per successful request; inter-token latency percentiles pool measured token intervals across successful requests. SLO goodput uses the successful-request time span.</p>
      <div class="chart-grid">
        <BenchmarkChart title="Time to first token" unit="seconds" series={ttftSeries} />
        <BenchmarkChart title="Inter-token latency" unit="seconds" series={itlSeries} />
        <BenchmarkChart title="Request throughput and SLO goodput" unit="requests/s" series={requestSeries} />
        <BenchmarkChart title="Output token throughput" unit="tokens/s" series={tokenSeries} />
      </div>
    </section>

    <section class="section">
      <h3>Stage measurements</h3>
      {#each views as view (view.result.run_id)}
        <h4>{view.label}: {view.result.run_id}</h4>
        <div class="table-wrap">
          <table>
            <thead><tr><th>Stage</th><th>Configured load</th><th>Observed request-start rate (requests/s)</th><th>Successes / failures</th><th>TTFT p50 / p95 (s)</th><th>ITL p50 / p95 (s)</th><th>Throughput (requests/s)</th><th>Output (tokens/s)</th><th>SLO goodput (requests/s)</th></tr></thead>
            <tbody>
              {#each view.result.points as point, index}
                <tr>
                  <td>{index + 1}</td>
                  <td>
                    {#if point.stage.type === 'constant'}
                      {point.stage.rate} requests/s target for {point.stage.duration} s
                    {:else}
                      {point.stage.concurrency_level} concurrent, {point.stage.num_requests} requests
                    {/if}
                  </td>
                  <td>{benchmarkNumber(point.achieved_request_rate)}</td>
                  <td>{point.successes} / {point.failures}</td>
                  <td>{benchmarkNumber(point.ttft_seconds?.p50)} / {benchmarkNumber(point.ttft_seconds?.p95)}</td>
                  <td>{benchmarkNumber(point.itl_seconds?.p50)} / {benchmarkNumber(point.itl_seconds?.p95)}</td>
                  <td>{benchmarkNumber(point.throughput.requests_per_second)}</td>
                  <td>{benchmarkNumber(point.throughput.output_tokens_per_second)}</td>
                  <td>{benchmarkNumber(point.goodput?.requests_per_second)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/each}
    </section>

    <section class="section">
      <h3>Run provenance</h3>
      <p class="muted">These are the recorded run identities and conditions, not a claim that they describe the current deployment. Commit, model revision or component digests not separately recorded here are unknown.</p>
      <div class="provenance-grid">
        {#each views as view (view.result.run_id)}
          <div class="panel provenance">
            <h4>{view.label}: {view.result.run_id}</h4>
            <dl>
              <dt>Service / deployment</dt><dd>{view.result.request.service} / {view.result.request.deployment}</dd>
              <dt>Project</dt><dd>{view.result.request.project_id}</dd>
              <dt>Desired configuration</dt><dd><code>{view.result.request.evidence.desired_configuration}</code></dd>
              <dt>Observed runtime</dt><dd><code>{view.result.request.evidence.observed_runtime}</code></dd>
              <dt>Test identity</dt><dd><code>{view.result.request.evidence.test_definition}</code></dd>
              <dt>Runner</dt><dd>{view.result.runner.name} {view.result.runner.version} · adapter {view.result.runner.adapter_version}</dd>
              <dt>Adapter image</dt><dd><code>{view.result.runner.image}</code></dd>
              <dt>Upstream runner image</dt><dd><code>{view.result.runner.upstream_image}</code></dd>
              <dt>Tokenizer</dt><dd>{view.result.tokenizer.repository} · <code>{view.result.tokenizer.revision}</code></dd>
              <dt>Workload</dt><dd>{view.result.definition.name} · seed {Number.isSafeInteger(view.result.definition.seed) ? view.result.definition.seed : 'Unknown (outside browser integer precision)'} · warmup {view.result.definition.warmup_seconds} s</dd>
              <dt>Goodput SLO</dt><dd>TTFT {benchmarkNumber(view.result.definition.spec.goodput.ttft)} s · ITL {benchmarkNumber(view.result.definition.spec.goodput.itl)} s</dd>
              <dt>Hardware / replicas</dt><dd>{view.result.request.evidence.conditions.hardware} / {view.result.request.evidence.conditions.replicas}</dd>
              <dt>Nodes</dt><dd>{view.result.request.evidence.conditions.nodes.join(', ')}</dd>
              <dt>Cache state</dt><dd>{view.result.request.evidence.conditions.cache_state}</dd>
              <dt>Competing traffic</dt><dd>{view.result.request.evidence.conditions.competing_traffic}</dd>
              <dt>Conditions observed at</dt><dd>{view.result.request.evidence.conditions.observed_at}</dd>
              <dt>Result schema</dt><dd>{view.result.schema_version}</dd>
            </dl>
            <details>
              <summary>Workload specification and run bounds</summary>
              <h5>Measurement specification</h5>
              <pre>{JSON.stringify(view.result.definition.spec, null, 2)}</pre>
              <h5>Requested run bounds</h5>
              <pre>{JSON.stringify({ ...view.result.request.workload, seed: Number.isSafeInteger(view.result.request.workload.seed) ? view.result.request.workload.seed : 'Unknown (outside browser integer precision)' }, null, 2)}</pre>
            </details>
          </div>
        {/each}
      </div>
    </section>
  {/if}
</section>

<style>
  .compare-form { margin-top: 20px; }
  .compare-form label { min-width: min(100%, 360px); flex: 1; }
  .chart-grid, .provenance-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 420px), 1fr)); gap: 16px; }
  .provenance { min-width: 0; }
  .provenance dl { display: grid; grid-template-columns: minmax(110px, 0.8fr) minmax(0, 1.5fr); gap: 10px 16px; font-size: 14px; }
  .provenance dt { color: var(--m3-on-surface-variant, #56655b); }
  .provenance dd { margin: 0; overflow-wrap: anywhere; }
  summary { cursor: pointer; }
  h4 { overflow-wrap: anywhere; }
</style>
