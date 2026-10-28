import { request } from './api';
import type { components } from './api.generated';

type Schemas = components['schemas'];

export function createRun(body: Schemas['CreateRun'], idempotencyKey: string): Promise<Schemas['RunView']> {
  return request('/runs', {
    method: 'POST',
    headers: { 'Idempotency-Key': idempotencyKey },
    body: JSON.stringify(body)
  });
}

export type RunPromotion = Schemas['RunPromotionView'];

export function promoteRun(id: string): Promise<RunPromotion> {
  return request(`/runs/${encodeURIComponent(id)}/promote`, { method: 'POST' });
}
