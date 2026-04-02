# Thalamus Admin Panel UI Implementation Plan

## Context

The UI project (`ui/`) is an Expo scaffold with only a home page and health check. The backend has 37+ fully implemented endpoints (auth, API keys, signing keys, authorization, LLM proxy). This plan builds a functional admin panel using GlueStack UI components + NativeWind, covering auth, key management, and authorization management.

## Phase 0: Infrastructure Setup

1. Run `pnpx gluestack-ui init --use-pnpm` in `ui/` to create config
2. Add needed GlueStack components: `pnpx gluestack-ui add button input select badge spinner toast alert alert-dialog modal card box vstack hstack heading text divider form-control checkbox textarea table icon pressable menu avatar tooltip center`
3. Install missing packages: `expo-secure-store`, `expo-clipboard`, `lucide-react-native`
4. Update package versions per Expo compatibility warnings

## Phase 1: Auth Foundation

**New files:**
- `src/lib/types.ts` — TypeScript types matching backend DTOs (ApiKeyInfo, PolicyResponse, RoleResponse, OAuthProviderInfo, WhoamiResponse, etc.)
- `src/lib/auth.ts` — Token storage: `getToken/setToken/clearToken` using localStorage (web) / expo-secure-store (native)
- `src/contexts/auth-context.tsx` — AuthProvider + `useAuth()` hook: holds user state from `/v1/auth/whoami`, login/logout functions, isAuthenticated/isLoading state
- `src/components/auth-guard.tsx` — Wraps authenticated routes, redirects to `/login` via Expo Router `<Redirect>`
- `src/services/auth.ts` — OAuth service: `getProviders()`, `startOAuthLogin()`, `whoami()`, `logout()`, `refreshToken()`

**Modified files:**
- `src/lib/api-client.ts` — Add auth header injection (reads token from auth.ts), 401 interceptor that attempts token refresh then retries

## Phase 2: Layout & Navigation

**Route structure:**
```
src/app/
  _layout.tsx         — Root: AuthProvider + QueryClientProvider + GlueStack
  login.tsx           — OAuth login page
  (admin)/
    _layout.tsx       — Auth guard + sidebar nav (web) / bottom tabs (native)
    index.tsx         — Dashboard
    api-keys/
      index.tsx       — List API keys
      create.tsx      — Create form, shows key once
    signing-keys/
      index.tsx       — List signing keys
      create.tsx      — Create form
    authorization/
      _layout.tsx     — Sub-tabs: policies | roles
      policies.tsx    — CRUD policies
      roles.tsx       — Assign/view/remove roles
    settings.tsx      — User info, refresh tokens, logout
```

**Admin layout:**
- Web: Fixed left sidebar with nav links (Dashboard, API Keys, Signing Keys, Authorization, Settings) + user menu at bottom
- Native: Bottom tabs (follow existing `app-tabs.tsx` / `app-tabs.web.tsx` pattern for platform-specific files)

**Reusable components:**
- `src/components/page-header.tsx` — Consistent page title + action button
- `src/components/status-badge.tsx` — Active/revoked/expired badge using GlueStack Badge
- `src/components/confirm-dialog.tsx` — Reusable AlertDialog wrapper
- `src/components/copy-button.tsx` — Copy text to clipboard with feedback
- `src/components/empty-state.tsx` — Empty list placeholder

## Phase 3: Dashboard

Rework `(admin)/index.tsx`:
- Health status card (migrate existing)
- User info card (user_id, team_id, roles from whoami)
- Quick stat cards: active API key count, backend version
- Quick action links to other sections

## Phase 4: API Keys Management

- `src/services/api-keys.ts` — `createApiKey()`, `listApiKeys()`, `revokeApiKey()`, `rotateApiKey()`
- `src/hooks/use-api-keys.ts` — React Query hooks wrapping service functions
- List page: table with name, prefix, scopes (badges), status, dates; row actions: rotate, revoke (with confirm dialog)
- Create page: form (name, description, scopes checkboxes, expiry), on success show key with copy button + warning

## Phase 5: Signing Keys Management

- `src/services/signing-keys.ts` — CRUD functions
- `src/hooks/use-signing-keys.ts` — React Query hooks
- List page: table with name, algorithm, fingerprint, use_count, status, dates
- Create page: form (algorithm select, name, description, scopes, expiry), on success show private+public key with copy + warning

## Phase 6: Authorization Management

- `src/services/authorization.ts` — Policies + roles CRUD
- `src/hooks/use-authorization.ts` — React Query hooks
- Policies tab: table (subject, domain, object, action) + create modal + delete with confirm
- Roles tab: lookup by user+domain, assign role form, remove role with confirm

## Phase 7: Settings & Polish

- Settings page: user info, refresh token list with revoke, logout button
- Toast notifications on all mutations
- Loading states with Spinner
- Dark mode verification

## Key Decisions

- **OAuth flow**: Call `/v1/auth/oauth/{provider}/login` to get authorization_url, redirect browser there. Backend callback returns token. Use `expo-web-browser` for native, direct redirect for web. May need a small `/oauth-callback` route to capture the token from the backend redirect.
- **Styling**: Use GlueStack components with NativeWind `className` props. Phase out manual `StyleSheet.create` in new code. Keep existing `ThemedText`/`ThemedView` working during transition but don't use them in new pages.
- **Auth state**: React Context wrapping the app. Token in memory + persisted. 401 → try refresh → redirect to login.
- **Scope gating**: Check user scopes from whoami and conditionally show/hide admin features.

## Verification

1. `pnpm web` should start without errors
2. Login page should show OAuth providers (or graceful empty state if backend isn't running)
3. After auth: dashboard shows health + user info
4. API keys CRUD works end-to-end
5. Authorization management works end-to-end
6. Responsive layout: sidebar on web, tabs on native
