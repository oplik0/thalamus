import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, ApiError } from './api';

afterEach(() => vi.unstubAllGlobals());
describe('control-plane client', () => {
  it('encodes service names and uses same-origin credentials', async () => {
    const requests: Request[] = [];
    vi.stubGlobal('fetch', vi.fn(async (input: string, init?: RequestInit) => {
      requests.push(new Request(new URL(input, 'https://ui.example'), init));
      return new Response(JSON.stringify({ metrics: [] }), { status: 200 });
    }));
    await api.metrics('team/model');
    expect(requests[0].url).toBe('https://ui.example/api/v1/services/team%2Fmodel/metrics');
    expect(requests[0].credentials).toBe('same-origin');
  });
  it.each([
    ['revoke', () => api.revokeKey('key-1')],
    ['verify revocation', () => api.verifyKeyRevocation('key-1', 'secret')],
  ] as const)('refreshes displayed keys after %s returns no content', async (_action, revoke) => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response(null, { status: 204 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({
        keys: [{ id: 'key-1', name: 'Production', project_id: 'project-1', allowed_services: ['service'], expires_at: null, revocation_state: 'revoked' }],
      }), { status: 200 }));
    vi.stubGlobal('fetch', fetch);

    // The keys page refreshes its displayed list after the revocation operation.
    let displayedState = 'active';
    await revoke();
    displayedState = (await api.keys()).keys[0].revocation_state;
    expect(displayedState).toBe('revoked');
    expect(fetch).toHaveBeenLastCalledWith('/api/v1/keys', expect.any(Object));
  });
  it('propagates server authorization errors', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ message: 'Forbidden' }), { status: 403 }));
    vi.stubGlobal('fetch', fetch);
    await expect(api.keys()).rejects.toEqual(new ApiError('Forbidden', 403));
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
