<script lang="ts">
  import { benchmarkChartSegments, benchmarkNumber, type BenchmarkChartSeries } from './BenchmarkChartData';

  export let title: string;
  export let unit: string;
  export let series: BenchmarkChartSeries[];

  $: prepared = series.map(line => ({ ...line, segments: benchmarkChartSegments(line.points) }));
  $: measuredPoints = prepared.flatMap(line => line.segments.flat());
  $: maxX = Math.max(0, ...measuredPoints.map(point => point.x));
  $: maxY = Math.max(0, ...measuredPoints.map(point => point.y));
  $: xScale = maxX > 0 ? maxX : 1;
  $: yScale = maxY > 0 ? maxY : 1;

  function x(value: number) { return 62 + value / xScale * 532; }
  function y(value: number) { return 174 - value / yScale * 152; }
  function tickLabel(value: number) { return benchmarkNumber(Number(value.toPrecision(5))); }
</script>

<div class="panel chart">
  <h3>{title}</h3>
  <p class="muted axis-unit">{unit}</p>
  {#if measuredPoints.length}
    <svg viewBox="0 0 620 226" role="img" aria-label={`${title}; ${unit} against observed request-start rate. Exact values are in the stage table.`}>
      <line x1="62" y1="174" x2="594" y2="174" class="axis" />
      <line x1="62" y1="22" x2="62" y2="174" class="axis" />
      {#each [0, 0.5, 1] as tick}
        <line x1="62" y1={174 - tick * 152} x2="594" y2={174 - tick * 152} class="gridline" />
        <text x="54" y={178 - tick * 152} text-anchor="end">{tickLabel(tick * yScale)}</text>
        <text x={62 + tick * 532} y="195" text-anchor="middle">{tickLabel(tick * xScale)}</text>
      {/each}
      {#each prepared as line}
        {#each line.segments as segment}
          {#if segment.length > 1}
            <polyline points={segment.map(point => `${x(point.x)},${y(point.y)}`).join(' ')} fill="none" stroke={line.color} stroke-width="2.5" stroke-dasharray={line.dashed ? '6 4' : undefined} />
          {/if}
          {#each segment as point}
            <circle cx={x(point.x)} cy={y(point.y)} r="3.5" fill={line.color}>
              <title>{line.label}, stage {point.stage}: observed request-start rate {benchmarkNumber(point.x)} requests/s; value {benchmarkNumber(point.y)} {unit}</title>
            </circle>
          {/each}
        {/each}
      {/each}
      <text x="328" y="219" text-anchor="middle">Observed request-start rate (requests/s)</text>
    </svg>
    <ul class="legend" aria-label="Chart legend">
      {#each series as line}
        <li><span class="legend-line" style={`border-top-color: ${line.color}; border-top-style: ${line.dashed ? 'dashed' : 'solid'}`}></span>{line.label}</li>
      {/each}
    </ul>
  {:else}
    <p class="muted">No stage has both an observed request-start rate and this measurement.</p>
  {/if}
</div>

<style>
  .chart { min-width: 0; }
  .axis-unit { margin: 0; font-size: 13px; }
  svg { width: 100%; height: auto; overflow: visible; margin-top: 8px; }
  svg text { font: 11px system-ui, sans-serif; fill: var(--m3-on-surface-variant, #56655b); }
  .axis { stroke: var(--m3-on-surface-variant, #56655b); stroke-width: 1; }
  .gridline { stroke: var(--m3-outline-variant, #d3ddd3); stroke-width: 0.8; }
  .legend { display: flex; flex-wrap: wrap; gap: 8px 18px; list-style: none; margin: 8px 0 0; padding: 0; font-size: 12px; }
  .legend li { display: flex; align-items: center; gap: 6px; }
  .legend-line { width: 20px; border-top-width: 2px; }
</style>
