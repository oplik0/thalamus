import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, ApiError } from './api';

afterEach(() => vi.unstubAllGlobals());
describe('control-plane client', () => {
  it('encodes service names and uses same-origin credentials', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ metrics: [] }), { status: 200 }));
    vi.stubGlobal('fetch', fetch);
    await api.metrics('team/model');
    expect(fetch).toHaveBeenCalledWith('/api/v1/services/team%2Fmodel/metrics', expect.objectContaining({ credentials: 'same-origin' }));
  });
  it('sends structured changes as JSON', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ id: 'c1', service: 'svc', state: 'proposed', base_commit: 'abc' }), { status: 200 }));
    vi.stubGlobal('fetch', fetch);
    await api.createChange({ service: 'svc', base_commit: 'abc', patch: { image: 'registry/image@sha256:abc' } });
    expect(fetch).toHaveBeenCalledWith('/api/v1/changes', expect.objectContaining({ method: 'POST', body: JSON.stringify({ service: 'svc', base_commit: 'abc', patch: { image: 'registry/image@sha256:abc' } }) }));
  });
  it('propagates server errors and accepts empty revocation responses', async () => {
    const fetch = vi.fn().mockResolvedValueOnce(new Response(JSON.stringify({ message: 'Forbidden' }), { status: 403 })).mockResolvedValueOnce(new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', fetch);
    await expect(api.keys()).rejects.toEqual(new ApiError('Forbidden', 403));
    await expect(api.revokeKey('id')).resolves.toBeUndefined();
  });
  it('shows a concise error instead of an HTML fallback page', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('<!doctype html><html><body>Not found</body></html>', { status: 404, headers: { 'Content-Type': 'text/html; charset=utf-8' } })));
    await expect(api.services()).rejects.toEqual(new ApiError('Request failed (404)', 404));
  });
  it('does not expose malformed JSON or markup served as plain text', async () => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response('{"message":', { status: 502, headers: { 'Content-Type': 'application/json' } }))
      .mockResolvedValueOnce(new Response('<html>Proxy error</html>', { status: 503, headers: { 'Content-Type': 'text/plain' } }));
    vi.stubGlobal('fetch', fetch);
    await expect(api.services()).rejects.toEqual(new ApiError('Request failed (502)', 502));
    await expect(api.services()).rejects.toEqual(new ApiError('Request failed (503)', 503));
  });
  it('preserves plain-text and JSON API errors', async () => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response('Service unavailable', { status: 503, headers: { 'Content-Type': 'text/plain' } }))
      .mockResolvedValueOnce(new Response(JSON.stringify({ error: 'Invalid service' }), { status: 400, headers: { 'Content-Type': 'application/json' } }));
    vi.stubGlobal('fetch', fetch);
    await expect(api.services()).rejects.toEqual(new ApiError('Service unavailable', 503));
    await expect(api.services()).rejects.toEqual(new ApiError('Invalid service', 400));
  });
});
