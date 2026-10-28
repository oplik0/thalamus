import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from './api';
import { projectTrafficApi } from './ProjectTrafficApi';

afterEach(() => vi.unstubAllGlobals());

describe('project traffic-class client', () => {
  it('preserves policy authorization failures instead of inventing a class list', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({ message: 'Forbidden' }), { status: 403 })));
    await expect(projectTrafficApi.classes('project-id')).rejects.toEqual(new ApiError('Forbidden', 403));
  });
});
