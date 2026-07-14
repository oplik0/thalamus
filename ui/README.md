# Thalamus UI

The Thalamus UI is an Expo web app for administering local and deployed Thalamus instances.

## Development

From the repository root, prefer the mise tasks:

```bash
mise install
mise run ui:dev
```

To run the backend and UI together:

```bash
mise run services:up
mise run db:migrate
mise run dev
```

`mise run dev` starts the backend with auto-reload and starts the web UI dev server in the background. UI logs are written to `.logs/ui-dev.log`.

If you are working inside `ui/` directly, use pnpm:

```bash
pnpm install
pnpm start --web
```

## Backend URL

The UI reads the backend URL from `EXPO_PUBLIC_API_URL` and defaults to `http://localhost:3000`.

For the full containerized test instance, the UI is exposed at `http://localhost:3020` and talks to the backend at `http://localhost:3000`.

## First-Run Setup and Login

On a fresh backend with no OAuth providers configured, the UI checks `/v1/auth/setup-status` and redirects to `/login/setup`. Create the first admin with a username, email, and password.

After setup, sign in with the username and password you created. The UI uses OPAQUE through `/v1/auth/login/start` and `/v1/auth/login/finish`; the password is not sent during normal login.

If OAuth providers are configured, setup is disabled and the login screen shows the configured providers in addition to username/password login.

## Useful Commands

| Command | Description |
|---|---|
| `mise run ui:dev` | Start Expo web dev server |
| `mise run dev` | Start backend auto-reload and UI dev server |
| `mise run ui:lint` | Run Biome checks |
| `mise run ui:format` | Format UI code |
| `pnpm start --web` | Start Expo web from `ui/` |
| `pnpm run lint` | Run Biome checks from `ui/` |
| `pnpm run format` | Format UI code from `ui/` |
