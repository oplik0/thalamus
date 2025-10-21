#!/usr/bin/env bash
set -euo pipefail

echo "Starting devenv services..."
devenv up -d

echo "Waiting for PostgreSQL to be ready..."
for i in {1..30}; do
    if pg_isready -h localhost -U postgres > /dev/null 2>&1; then
        echo "PostgreSQL is ready!"
        break
    fi
    if [ $i -eq 30 ]; then
        echo "Timeout waiting for PostgreSQL"
        exit 1
    fi
    sleep 1
done

echo "Waiting for Redis to be ready..."
for i in {1..30}; do
    if redis-cli -h localhost ping > /dev/null 2>&1; then
        echo "Redis is ready!"
        break
    fi
    if [ $i -eq 30 ]; then
        echo "Timeout waiting for Redis"
        exit 1
    fi
    sleep 1
done

echo "Test environment is ready!"
