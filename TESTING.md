# Basic Testing Guide

## Step 1: setup ollama

Currently, regretably, the example configuration includes connection for ollama, due to its set up simplicity.

In the future, [for multiple reasons](https://sleepingrobots.com/dreams/stop-using-ollama/), including [security](https://www.striga.ai/research/ollama-windows-auto-update-rce), we will be recommending using  different engines - most likely llama.cpp for a simple dev setup. However, for now the ollama configuration was actually tested to work.

## Step 2: Create configuration

Copy `config.k.example` into `config.k`.

Make sure the oauth provider config for the mock-oidc provider is not commented out or set up your own oidc or github oauth provider.

## Step 3: start docker compose

Simplest way to test a full setup is via docker compose, simply run `docker compose up --build` to ensure you are using current code.

## Step 4: test UI

Go to http://localhost:3020 for an admin panel. Log in with email `test@example.com` and password `zaq1@WSX`
