import { request } from './api';
import type { components } from './api.generated';

type Schemas = components['schemas'];
export type DebugCreateRequest = Schemas['DebugSessionCreateRequest'];
export type DebugReplyRequest = Schemas['DebugReplyRequest'];
export type DebugSession = Schemas['DebugSessionResponse'];
export type DebugEvent = Schemas['DebugEventResponse'];
export type DebugService = Schemas['ServiceView'];
export type DebugProject = Schemas['ProjectView'];
export type DebugRun = Schemas['RunView'];

const segment = encodeURIComponent;
const teamQuery = (teamId: string) => new URLSearchParams({ team_id: teamId });

export const debugApi = {
  services: (signal: AbortSignal) => request<Schemas['ServiceList']>('/services', { signal }),
  projects: (signal: AbortSignal) => request<Schemas['ProjectsResponse']>('/projects', { signal }),
  models: (projectId: string, signal: AbortSignal) =>
    request<Schemas['DebugModelsResponse']>(`/debug/models?${new URLSearchParams({ project_id: projectId })}`, { signal }),
  run: (id: string, signal: AbortSignal) => request<DebugRun>(`/runs/${segment(id)}`, { signal }),
  create: (body: DebugCreateRequest, signal: AbortSignal) => request<DebugSession>('/debug/sessions', {
    method: 'POST', body: JSON.stringify(body), signal
  }),
  session: (id: string, teamId: string, signal: AbortSignal) =>
    request<DebugSession>(`/debug/sessions/${segment(id)}?${teamQuery(teamId)}`, { signal }),
  events: (id: string, teamId: string, after: number, limit: number, signal: AbortSignal) => {
    const query = teamQuery(teamId);
    query.set('after', String(after));
    query.set('limit', String(limit));
    return request<DebugEvent[]>(`/debug/sessions/${segment(id)}/events?${query}`, { signal });
  },
  reply: (id: string, teamId: string, body: DebugReplyRequest, signal: AbortSignal) =>
    request<void>(`/debug/sessions/${segment(id)}/reply?${teamQuery(teamId)}`, {
      method: 'POST', body: JSON.stringify(body), signal
    }),
  end: (id: string, teamId: string, signal: AbortSignal) =>
    request<void>(`/debug/sessions/${segment(id)}?${teamQuery(teamId)}`, { method: 'DELETE', signal })
};
