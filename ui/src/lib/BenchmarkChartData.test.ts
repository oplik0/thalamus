import { describe, expect, it } from 'vitest';
import type { BenchmarkResult } from './BenchmarkApi';
import { benchmarkChartSegments, benchmarkChartSeries, benchmarkNumber } from './BenchmarkChartData';

type Point = BenchmarkResult['points'][number];
const point = (overrides: Partial<Point> = {}): Point => ({
  stage: { type: 'constant', rate: 10, duration: 60 },
  successes: 12,
  failures: 0,
  achieved_request_rate: 8,
  ttft_seconds: { p50: 0.1, p95: 0.2 },
  itl_seconds: { p50: 0.01, p95: 0.02 },
  throughput: { requests_per_second: 7, output_tokens_per_second: 30 },
  goodput: { requests_per_second: 6 },
  ...overrides
});
// Chart calculation only reads points; the server supplies the remaining run provenance.
const result = (points: Point[]): BenchmarkResult => ({ points } as BenchmarkResult);

describe('benchmark chart data', () => {
  it('uses observed request-start rates and separate percentile series', () => {
    const series = benchmarkChartSeries([{ label: 'Before', result: result([
      point(),
      point({ achieved_request_rate: 16, ttft_seconds: { p50: 0.3, p95: 0.7 } })
    ]) }], 'ttft');
    expect(series.map(item => item.label)).toEqual(['Before p50', 'Before p95']);
    expect(series[0].points).toEqual([{ x: 8, y: 0.1, stage: 1 }, { x: 16, y: 0.3, stage: 2 }]);
    expect(series[1].points).toEqual([{ x: 8, y: 0.2, stage: 1 }, { x: 16, y: 0.7, stage: 2 }]);
    expect(series[0].dashed).toBe(false);
    expect(series[1].dashed).toBe(true);
  });

  it('does not invent request rates from configured load or turn null into zero', () => {
    const series = benchmarkChartSeries([{ label: 'Run', result: result([
      point({ achieved_request_rate: null, goodput: null }),
      point({ stage: { type: 'concurrent', concurrency_level: 32, num_requests: 100 }, achieved_request_rate: 0,
        throughput: { requests_per_second: 0, output_tokens_per_second: null }, goodput: null })
    ]) }], 'request-rates');
    expect(series[0].points).toEqual([{ x: null, y: 7, stage: 1 }, { x: 0, y: 0, stage: 2 }]);
    expect(series[1].points).toEqual([{ x: null, y: null, stage: 1 }, { x: 0, y: null, stage: 2 }]);
    expect(benchmarkNumber(null)).toBe('Unknown');
    expect(benchmarkNumber(Number.NaN)).toBe('Unknown');
    expect(benchmarkNumber(0)).toBe('0');
  });

  it('breaks curves at missing measurements instead of interpolating', () => {
    expect(benchmarkChartSegments([
      { x: 3, y: 0.3, stage: 1 },
      { x: 1, y: 0.1, stage: 2 },
      { x: 2, y: null, stage: 3 },
      { x: null, y: 0.4, stage: 4 },
      { x: 4, y: 0.5, stage: 5 }
    ])).toEqual([
      [{ x: 1, y: 0.1, stage: 2 }, { x: 3, y: 0.3, stage: 1 }],
      [{ x: 4, y: 0.5, stage: 5 }]
    ]);
  });

  it('keeps measured token throughput and absent token throughput distinct', () => {
    const series = benchmarkChartSeries([{ label: 'Run', result: result([
      point({ throughput: { requests_per_second: 2, output_tokens_per_second: 0 } }),
      point({ achieved_request_rate: 9, throughput: { requests_per_second: 3, output_tokens_per_second: null } })
    ]) }], 'output-tokens');
    expect(series).toHaveLength(1);
    expect(series[0].points).toEqual([{ x: 8, y: 0, stage: 1 }, { x: 9, y: null, stage: 2 }]);
  });
});
