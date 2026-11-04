import { request } from './api';
import type { components } from './api.generated';

type Schemas = components['schemas'];
export type ChangeServiceForm = Schemas['ServiceForm'];
export type ChangeRequest = Schemas['CreateChange'];
export type ChangeRolloutView = Schemas['RolloutView'];
export type ChangeRolloutSchedule = Schemas['ScheduleRollout'];
export type ChangeApprovalBinding = NonNullable<ChangeRolloutView['approval_binding']>;

export function getChangeForm(service: string, signal?: AbortSignal): Promise<ChangeServiceForm> {
  return request<ChangeServiceForm>(`/changes/form/${encodeURIComponent(service)}`, { signal });
}

export function createChange(body: ChangeRequest, idempotencyKey: string, signal?: AbortSignal): Promise<Schemas['ChangeView']> {
  return request<Schemas['ChangeView']>('/changes', {
    method: 'POST',
    headers: { 'Idempotency-Key': idempotencyKey },
    body: JSON.stringify(body),
    signal
  });
}

export function getChangeRollout(id: string, signal?: AbortSignal): Promise<ChangeRolloutView> {
  return request<ChangeRolloutView>(`/changes/${encodeURIComponent(id)}/rollout`, { signal });
}

export function scheduleChangeRollout(id: string, body: ChangeRolloutSchedule, signal?: AbortSignal): Promise<ChangeRolloutView> {
  return request<ChangeRolloutView>(`/changes/${encodeURIComponent(id)}/rollout/schedule`, {
    method: 'POST',
    body: JSON.stringify(body),
    signal
  });
}

export function approveChangeRollout(id: string, binding: ChangeApprovalBinding, signal?: AbortSignal): Promise<ChangeRolloutView> {
  return request<ChangeRolloutView>(`/changes/${encodeURIComponent(id)}/rollout/approve`, {
    method: 'POST',
    body: JSON.stringify(binding),
    signal
  });
}
