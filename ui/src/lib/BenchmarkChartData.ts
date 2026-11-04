import type { BenchmarkResult } from './BenchmarkApi';

export type BenchmarkMetric = 'ttft' | 'itl' | 'request-rates' | 'output-tokens';
export type BenchmarkChartPoint = { x: number | null; y: number | null; stage: number };
export type BenchmarkMeasuredPoint = { x: number; y: number; stage: number };
export type BenchmarkChartSeries = {
  label: string;
  color: string;
  dashed: boolean;
  points: BenchmarkChartPoint[];
};

const measured = (value: number | null | undefined): number | null =>
  value != null && Number.isFinite(value) && value >= 0 ? value : null;

/** Plot observed request-start rate; configured rate or concurrency is not an observation. */
export function benchmarkChartSeries(
  results: { label: string; result: BenchmarkResult }[],
  metric: BenchmarkMetric
): BenchmarkChartSeries[] {
  const colors = ['#315e4b', '#a0523b'];
  return results.flatMap(({ label, result }, index) => {
    const color = colors[index % colors.length];
    const series = (name: string, dashed: boolean, value: (point: BenchmarkResult['points'][number]) => number | null | undefined): BenchmarkChartSeries => ({
      label: `${label} ${name}`,
      color,
      dashed,
      points: result.points.map((point, stage) => ({
        x: measured(point.achieved_request_rate),
        y: measured(value(point)),
        stage: stage + 1
      }))
    });
    switch (metric) {
      case 'ttft':
        return [series('p50', false, point => point.ttft_seconds?.p50), series('p95', true, point => point.ttft_seconds?.p95)];
      case 'itl':
        return [series('p50', false, point => point.itl_seconds?.p50), series('p95', true, point => point.itl_seconds?.p95)];
      case 'request-rates':
        return [series('throughput', false, point => point.throughput.requests_per_second), series('SLO goodput', true, point => point.goodput?.requests_per_second)];
      case 'output-tokens':
        return [series('output throughput', false, point => point.throughput.output_tokens_per_second)];
    }
  });
}

/** Missing load or metric values break a line rather than implying an interpolated measurement. */
export function benchmarkChartSegments(points: BenchmarkChartPoint[]): BenchmarkMeasuredPoint[][] {
  const segments: BenchmarkMeasuredPoint[][] = [];
  let segment: BenchmarkMeasuredPoint[] = [];
  for (const point of points) {
    if (point.x == null || point.y == null) {
      if (segment.length) segments.push(segment.sort((a, b) => a.x - b.x));
      segment = [];
    } else {
      segment.push({ x: point.x, y: point.y, stage: point.stage });
    }
  }
  if (segment.length) segments.push(segment.sort((a, b) => a.x - b.x));
  return segments;
}

export function benchmarkNumber(value: number | null | undefined): string {
  return value == null || !Number.isFinite(value) ? 'Unknown' : value.toString();
}
