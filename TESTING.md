# Basic Testing Guide

This guide covers the fastest ways to run a local test instance and verify the UI, authentication, and LLM proxy path.

## Local Dev Instance

Use this path when actively developing the backend or UI.

```bash
mise install
mise run services:up
mise run db:migrate
mise run dev
```

`mise run dev` starts the backend with auto-reload and starts the web UI dev server. The backend listens on `http://localhost:3000`. The UI dev server URL is printed by Expo and also logs to `.logs/ui-dev.log`.

If you only need the backend, run:

```bash
mise run dev:server
```

If you only need the UI, run:

```bash
mise run ui:dev
```

## Full Container Test Instance

Use this path when you want a clean containerized stack.

```bash
cp config.k.example config.k
docker compose up --build
```

The full compose stack exposes:

- Backend: `http://localhost:3000`
- UI: `http://localhost:3020`
- Mock OIDC provider: `http://localhost:9999`

The example config leaves OAuth providers commented out by default so first-run username/password setup works. If you specifically want to test OAuth, uncomment the `mock-oidc` provider in `config.k` and restart the stack.

## First Admin Setup

On a fresh database with no OAuth providers configured, setup is required:

```bash
curl http://localhost:3000/v1/auth/setup-status
```

Expected response:

```json
{"needs_setup":true}
```

Create the first admin through the UI setup screen, or call the setup endpoint directly:

```bash
curl -X POST http://localhost:3000/v1/auth/setup \
  -H "Content-Type: application/json" \
  -d '{
    "username": "admin",
    "email": "admin@example.com",
    "password": "SuperSecret1!"
  }'
```

The response includes a PASETO token. Use that token as `Authorization: Bearer <token>` for authenticated API calls.

After setup, sign in through the UI with the username and password you created. The UI uses OPAQUE through `/v1/auth/login/start` and `/v1/auth/login/finish`; there is no fixed development password.

## Create an API Key

Use the setup token or a token from username/password login:

```bash
curl -X POST http://localhost:3000/v1/api-keys \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_PASETO_TOKEN" \
  -d '{
    "name": "test-key",
    "scopes": ["llm:chat", "llm:completions"]
  }'
```

The API returns the raw API key once. Store it somewhere temporary for testing.

## Test Ollama Routing

The default config routes to local Ollama. Start Ollama and pull the configured model before testing chat completions:

```bash
ollama pull qwen3.5:2b
ollama serve
```

Then send a request through Thalamus:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -d '{
    "model": "qwen3.5:2b",
    "messages": [
      {"role": "user", "content": "Say hello from Thalamus."}
    ]
  }'
```

## Reset State

For local dev services:

```bash
mise run db:reset
```

For the full compose stack:

```bash
docker compose down -v
```
