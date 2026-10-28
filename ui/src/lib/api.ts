// Same-origin, cookie-authenticated control-plane API. Authorization is server-side.
import type { components } from './api.generated';

type Schemas = components['schemas'];
export type Service = Schemas['ServiceView'];
export type Observation = Service['status']['accepted'];
export type Metric = Schemas['MetricList']['metrics'][number];
export type Project = Schemas['ProjectView'];
export type Key = Schemas['KeyView'];
export type Change = Schemas['ChangeView'];
export type Validation = Schemas['ValidationView'];
export type Run = Schemas['RunView'];
export type UsageHistory = Schemas['UsageResponse'];
export type UsageRollup = UsageHistory['usage'][number];

export class ApiError extends Error { constructor(message: string, public status: number) { super(message); } }
export async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(`/api/v1${path}`, { ...init, credentials: 'same-origin', headers: { ...(init.body ? { 'Content-Type': 'application/json' } : {}), ...init.headers } });
  if (!response.ok) {
    const text = await response.text();
    const fallback = `Request failed (${response.status})`;
    const contentType = response.headers.get('content-type')?.split(';', 1)[0].trim().toLowerCase();
    let message = fallback;
    if (contentType !== 'text/html' && contentType !== 'application/xhtml+xml') {
      try {
        const body = JSON.parse(text);
        const detail = body && typeof body === 'object' ? body.message || body.error : undefined;
        if (typeof detail === 'string' && detail.trim() && !/^\s*</.test(detail)) message = detail;
      } catch {
        if (contentType === 'text/plain' && text.trim() && !/^\s*</.test(text)) message = text;
      }
    }
    throw new ApiError(message, response.status);
  }
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}
const post = <T>(path: string, body: unknown) => request<T>(path, { method: 'POST', body: JSON.stringify(body) });
const segment = encodeURIComponent;
export const api = {
  services: () => request<Schemas['ServiceList']>('/services'),
  service: (id: string) => request<Service>(`/services/${segment(id)}`),
  metrics: (id: string) => request<Schemas['MetricList']>(`/services/${segment(id)}/metrics`),
  projects: () => request<Schemas['ProjectsResponse']>('/projects'),
  usage: (team: string, start: string, end: string, projectId?: string) => {
    const query = new URLSearchParams({ team, start, end });
    if (projectId) query.set('project_id', projectId);
    return request<UsageHistory>(`/usage?${query}`);
  },
  createProject: (body: Schemas['CreateProject']) => post<Project>('/projects', body),
  keys: () => request<Schemas['KeysResponse']>('/keys'),
  createKey: (body: Schemas['CreateKey']) => post<Schemas['IssuedKeyView']>('/keys', body),
  revokeKey: (id: string) => request<void>(`/keys/${segment(id)}`, { method: 'DELETE' }),
  verifyKeyRevocation: (id: string, key: string) => post<void>(`/keys/${segment(id)}/verify-revocation`, { key }),
  clientConfig: (body: Schemas['ConfigRequest']) => post<Schemas['ConfigResponse']>('/client-config', body),
  checkClient: (body: Schemas['CheckRequest']) => post<Schemas['CheckResponse']>('/client-config/check', body),
  changes: () => request<Schemas['ChangesView']>('/changes'),
  createChange: (body: Schemas['CreateChange']) => post<Change>('/changes', body),
  validateChange: (id: string) => post<Validation>(`/changes/${segment(id)}/validate`, {}),
  refreshChange: (id: string) => post<Change>(`/changes/${segment(id)}/refresh`, {}),
  openPr: (id: string) => post<Schemas['PrView']>(`/changes/${segment(id)}/open-pr`, {}),
  runs: () => request<Schemas['RunsView']>('/runs'),
  run: (id: string) => request<Run>(`/runs/${segment(id)}`),
  createRun: (body: Schemas['CreateRun']) => post<Run>('/runs', body)
};
export const errorMessage = (error: unknown) => error instanceof Error ? error.message : 'An unexpected error occurred.';
