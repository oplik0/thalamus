import { request } from './api';
import type { components } from './api.generated';

type Schemas = components['schemas'];
export type BenchmarkResult = Schemas['BenchmarkResult'];
export type BenchmarkComparison = Schemas['BenchmarkComparison'];

export const benchmarkApi = {
  result: (id: string, signal?: AbortSignal) => request<BenchmarkResult>(`/runs/${encodeURIComponent(id)}/benchmark`, { signal }),
  compare: (baseline: string, candidate: string, signal?: AbortSignal) =>
    request<BenchmarkComparison>(`/runs/${encodeURIComponent(baseline)}/benchmark/compare/${encodeURIComponent(candidate)}`, { signal })
};
