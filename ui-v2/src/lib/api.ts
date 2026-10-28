// Same-origin, cookie-authenticated control-plane API. Authorization is server-side.
export type Observation = { state: 'unknown' | 'stale' | 'true' | 'false'; observed_at: string | null; reason: string | null };
export type Service = { name: string; description?: string; deployments: { name: string; recipe: string; artifact: string; hardware: string }[]; status: Record<'accepted' | 'applied' | 'running' | 'functional' | 'measured', Observation> };
export type Metric = { deployment: string; name: string; value: number | null; unit: string; observed_at: string | null };
export type Project = { id: string; name: string; team: string; budget: number | null };
export type Key = { id: string; name: string; project_id: string; allowed_services: string[]; expires_at: string | null; revocation_state: string };
export type Change = { id: string; service: string; state: string; base_commit: string };
export type Validation = { data_diff: string; rendered_diff: string; errors: string[] };
export type Run = { id: string; service: string; deployment: string; kind: string; state: string; revision: string };

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
  services: () => request<{ services: Service[] }>('/services'),
  service: (id: string) => request<Service>(`/services/${segment(id)}`),
  metrics: (id: string) => request<{ metrics: Metric[] }>(`/services/${segment(id)}/metrics`),
  projects: () => request<{ projects: Project[] }>('/projects'),
  createProject: (body: { name: string; team: string; budget: number | null }) => post<Project>('/projects', body),
  keys: () => request<{ keys: Key[] }>('/keys'),
  createKey: (body: { name: string; project_id: string; allowed_services: string[]; expires_at: string | null }) => post<{ key: Key; secret: string }>('/keys', body),
  revokeKey: (id: string) => request<void>(`/keys/${segment(id)}`, { method: 'DELETE' }),
  clientConfig: (body: { service: string; client: string; major_version: string }) => post<{ config: string }>('/client-config', body),
  checkClient: (body: { service: string; client: string; major_version: string; key: string }) => post<{ ok: boolean; detail: string }>('/client-config/check', body),
  changes: () => request<{ changes: Change[] }>('/changes'),
  createChange: (body: { service: string; base_commit: string; patch: Record<string, unknown> }) => post<Change>('/changes', body),
  validateChange: (id: string) => post<Validation>(`/changes/${segment(id)}/validate`, {}),
  openPr: (id: string) => post<{ url: string }>(`/changes/${segment(id)}/open-pr`, {}),
  runs: () => request<{ runs: Run[] }>('/runs'),
  run: (id: string) => request<Run>(`/runs/${segment(id)}`),
  createRun: (body: { service: string; deployment: string; kind: string }) => post<Run>('/runs', body)
};
export const errorMessage = (error: unknown) => error instanceof Error ? error.message : 'An unexpected error occurred.';
