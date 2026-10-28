import { request } from './api';
import type { components } from './api.generated';

type Schemas = components['schemas'];

export const projectTrafficApi = {
  classes: (id: string, signal?: AbortSignal) =>
    request<Schemas['ProjectTrafficClasses']>(`/projects/${encodeURIComponent(id)}/traffic-classes`, { signal }),
  set: (id: string, body: Schemas['SetProjectTrafficClass'], signal?: AbortSignal) =>
    request<void>(`/projects/${encodeURIComponent(id)}/traffic-class`, {
      method: 'PUT', body: JSON.stringify(body), signal
    })
};
