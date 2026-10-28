#!/usr/bin/env bash
# Default: isolated network, no published ports, automatic cleanup.
# --browser: publish HTTPS on loopback and retain services/private SMOKE_DIR.
# --retain-on-failure requires --browser and keeps failed state for diagnosis.
# --cleanup <SMOKE_DIR>: remove a retained browser fixture and its data.
# Requires Docker Compose, git, openssl, python3 and timeout.
set -euo pipefail
umask 077
here="$(dirname "$(realpath "$0")")"
if [[ "${1:-}" == "--cleanup" ]]; then
    [[ $# == 2 ]] || { echo 'Usage: run.sh --cleanup <SMOKE_DIR>' >&2; exit 2; }
    retained="$(realpath -- "$2")"
    [[ "${retained##*/}" == thalamus-oidc-smoke.* && -f "$retained/compose.env" && -f "$retained/project" ]] || {
        echo 'Not a retained OIDC smoke directory' >&2; exit 2;
    }
    project="$(cat "$retained/project")"
    [[ "$project" =~ ^thalamusoidc[0-9]+$ ]] || { echo 'Invalid fixture project' >&2; exit 2; }
    SMOKE_DIR="$retained" docker compose --project-name "$project" --env-file "$retained/compose.env" \
        -f "$here/compose.yaml" -f "$here/publish.yaml" down --volumes --remove-orphans --rmi local
    rm -rf -- "$retained"
    exit 0
fi
browser=0
retain_failure=0
while (( $# )); do
    case "$1" in
        --browser) browser=1 ;;
        --retain-on-failure) retain_failure=1 ;;
        *) echo 'Usage: run.sh [--browser [--retain-on-failure] | --cleanup <SMOKE_DIR>]' >&2; exit 2 ;;
    esac
    shift
done
if (( retain_failure && !browser )); then
    echo '--retain-on-failure requires --browser' >&2
    exit 2
fi
export SMOKE_RETAIN_FAILURE="$retain_failure"
export SMOKE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/thalamus-oidc-smoke.XXXXXXXX")"
export COMPOSE_PROJECT_NAME="thalamusoidc${RANDOM}$$"
printf '%s\n' "$COMPOSE_PROJECT_NAME" > "$SMOKE_DIR/project"
if (( browser )); then
    export SMOKE_PORT="${SMOKE_PORT:-18443}"
    [[ "$SMOKE_PORT" =~ ^[1-9][0-9]{0,4}$ ]] && (( SMOKE_PORT >= 1024 && SMOKE_PORT <= 65535 )) || {
        echo 'SMOKE_PORT must be between 1024 and 65535' >&2
        rm -rf -- "$SMOKE_DIR"
        exit 2
    }
    export SMOKE_IDP_HOST=idp.localhost SMOKE_APP_HOST=app.localhost
    export SMOKE_IDP_ORIGIN="https://${SMOKE_IDP_HOST}:${SMOKE_PORT}"
    export SMOKE_APP_ORIGIN="https://${SMOKE_APP_HOST}:${SMOKE_PORT}"
    compose=(docker compose -f "$here/compose.yaml" -f "$here/publish.yaml")
else
    export SMOKE_PORT=443 SMOKE_IDP_HOST=idp.test SMOKE_APP_HOST=app.test
    export SMOKE_IDP_ORIGIN="https://${SMOKE_IDP_HOST}"
    export SMOKE_APP_ORIGIN="https://${SMOKE_APP_HOST}"
    compose=(docker compose -f "$here/compose.yaml")
fi
retain=0
started=0

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if (( status == 0 && retain )); then
        printf 'Retained browser fixture directory: %s\n' "$SMOKE_DIR"
        exit 0
    fi
    if (( status != 0 )); then
        echo "OIDC smoke failed; recent service logs follow:" >&2
        "${compose[@]}" logs --no-color --tail 100 postgres keycloak proxy server >&2 || true
        if (( retain_failure && started )) && [[ -f "$SMOKE_DIR/compose.env" ]]; then
            printf 'Failed browser fixture retained for diagnosis: %s\n' "$SMOKE_DIR" >&2
            exit "$status"
        fi
    fi
    "${compose[@]}" down --volumes --remove-orphans --rmi local >/dev/null 2>&1 || true
    rm -rf -- "$SMOKE_DIR"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

export SMOKE_DB_PASSWORD="$(openssl rand -hex 24)"
export SMOKE_ADMIN_PASSWORD="$(openssl rand -hex 24)"
export SMOKE_USER_PASSWORD="$(openssl rand -hex 24)"
export SMOKE_INTERACTIVE_PASSWORD="$(openssl rand -hex 24)"
export SMOKE_BROWSER_SECRET="$(openssl rand -hex 24)"
export SMOKE_API_SECRET="$(openssl rand -hex 24)"
export SMOKE_MCP_SECRET="$(openssl rand -hex 24)"
export SMOKE_SHORT_SECRET="$(openssl rand -hex 24)"
export SMOKE_WRONG_SECRET="$(openssl rand -hex 24)"
export SMOKE_COLLECTOR_SECRET="$(openssl rand -hex 24)"
export SMOKE_SESSION_KEY="$(openssl rand -hex 32)"

openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 1 \
    -keyout "$SMOKE_DIR/ca.key" -out "$SMOKE_DIR/ca.pem" -subj '/CN=Thalamus OIDC smoke CA' \
    -addext 'basicConstraints=critical,CA:TRUE,pathlen:0' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' >/dev/null 2>&1
openssl req -new -newkey rsa:2048 -nodes -sha256 \
    -keyout "$SMOKE_DIR/server.key" -out "$SMOKE_DIR/server.csr" -subj "/CN=${SMOKE_IDP_HOST}" >/dev/null 2>&1
printf '%s\n' 'basicConstraints=critical,CA:FALSE' 'keyUsage=critical,digitalSignature,keyEncipherment' \
    'extendedKeyUsage=serverAuth' "subjectAltName=DNS:${SMOKE_IDP_HOST},DNS:${SMOKE_APP_HOST}" > "$SMOKE_DIR/leaf.ext"
openssl x509 -req -sha256 -days 1 -in "$SMOKE_DIR/server.csr" -CA "$SMOKE_DIR/ca.pem" \
    -CAkey "$SMOKE_DIR/ca.key" -CAcreateserial -out "$SMOKE_DIR/server.crt" \
    -extfile "$SMOKE_DIR/leaf.ext" >/dev/null 2>&1
printf '%s\n' 'not a PEM certificate' > "$SMOKE_DIR/invalid.pem"
# Generate the complete example catalog and grant the fixture team one service.
python3 "$here/provision.py"
git -C "$SMOKE_DIR/repo" init -q
git -C "$SMOKE_DIR/repo" config user.name 'OIDC smoke fixture'
git -C "$SMOKE_DIR/repo" config user.email 'smoke@example.invalid'
git -C "$SMOKE_DIR/repo" add -f catalog
git -C "$SMOKE_DIR/repo" -c core.hooksPath=/dev/null -c commit.gpgsign=false commit -qm 'Fixture access policy'
export SMOKE_CATALOG_COMMIT="$(git -C "$SMOKE_DIR/repo" rev-parse HEAD)"
chmod -R a+rX "$SMOKE_DIR/repo"

# Compose needs these values again for explicit cleanup of a retained fixture.
for name in SMOKE_DIR COMPOSE_PROJECT_NAME SMOKE_PORT SMOKE_IDP_HOST SMOKE_APP_HOST \
    SMOKE_IDP_ORIGIN SMOKE_APP_ORIGIN SMOKE_CATALOG_COMMIT SMOKE_DB_PASSWORD \
    SMOKE_ADMIN_PASSWORD SMOKE_BROWSER_SECRET SMOKE_API_SECRET SMOKE_MCP_SECRET \
    SMOKE_COLLECTOR_SECRET SMOKE_SESSION_KEY; do
    printf '%s=%s\n' "$name" "${!name}" >> "$SMOKE_DIR/compose.env"
done
# The service containers run as unprivileged users; the parent temp directory
# remains private and the CA and realm are mounted directly as read-only files.
chmod 644 "$SMOKE_DIR/ca.pem" "$SMOKE_DIR/server.crt" "$SMOKE_DIR/invalid.pem" "$SMOKE_DIR/realm.json" "$SMOKE_DIR/nginx.conf"

"${compose[@]}" build server collector-test
"${compose[@]}" up -d postgres keycloak proxy
started=1
"${compose[@]}" run --rm --no-deps -T client wait-idp
"${compose[@]}" up -d server
"${compose[@]}" run --rm --no-deps -T client wait-app

# A second otherwise-identical server cannot discover this provider without the
# explicit CA; timeouts and unrelated startup errors are not accepted as success.
set +e
timeout 30s "${compose[@]}" run --rm --no-deps -T --entrypoint /bin/sh server \
    -c 'unset THALAMUS_OIDC_CA_FILE; exec thalamus-server' > "$SMOKE_DIR/no-ca.log" 2>&1
status=$?
set -e
if (( status == 0 || status == 124 )); then
    echo 'Server did not fail closed without the IdP CA' >&2
    exit 1
fi
case "$(cat "$SMOKE_DIR/no-ca.log")" in
    *'Oidc('*|*'OIDC error'*) ;;
    *) echo 'Server failed before OIDC discovery in the no-CA check' >&2; cat "$SMOKE_DIR/no-ca.log" >&2; exit 1 ;;
esac

# A configured but malformed CA must not silently fall back to public roots.
set +e
timeout 30s "${compose[@]}" run --rm --no-deps -T -e THALAMUS_OIDC_CA_FILE=/certs/invalid.pem server \
    > "$SMOKE_DIR/invalid-ca.log" 2>&1
status=$?
set -e
if (( status == 0 || status == 124 )); then
    echo 'Server did not reject malformed CA configuration' >&2
    exit 1
fi
case "$(cat "$SMOKE_DIR/invalid-ca.log")" in
    *'invalid MCP introspection configuration'*) ;;
    *) echo 'Server failed for an unexpected reason in the invalid-CA check' >&2; cat "$SMOKE_DIR/invalid-ca.log" >&2; exit 1 ;;
esac

# Seed only the team named in the group mapping. The user and membership are
# subsequently created by the real Keycloak authorization-code/PKCE login.
"${compose[@]}" exec -T postgres psql -X -v ON_ERROR_STOP=1 -U thalamus -d thalamus -c \
    "INSERT INTO identity_teams(id,name) VALUES ('3b3d3f25-f437-4f1a-bff5-36f6fac3714c','smoke-team')" >/dev/null
"${compose[@]}" run --rm --no-deps -T client run

# Disable at the real IdP first; the local browser session remains usable until
# the real collector reads Keycloak and applies the offboarding snapshot.
"${compose[@]}" run --rm --no-deps -T client disable-idp
"${compose[@]}" run --rm --no-deps -T collector-test | tee "$SMOKE_DIR/collector-test.log"
case "$(cat "$SMOKE_DIR/collector-test.log")" in
    *'keycloak_offboarding_revokes_existing_keys ... ok'*) ;;
    *) echo 'Collector offboarding test was not executed successfully' >&2; exit 1 ;;
esac
revoked="$("${compose[@]}" exec -T postgres psql -X -v ON_ERROR_STOP=1 -t -A -U thalamus -d thalamus -c \
    "SELECT count(*) FROM identity_keys WHERE name='smoke-key' AND revoked_at IS NOT NULL AND user_id=(SELECT id FROM identity_users WHERE issuer='${SMOKE_IDP_ORIGIN}/realms/thalamus' AND subject='54ac7e34-503a-446a-88b1-4884ea903114')")"
[[ "$revoked" == 1 ]] || { echo 'Collector did not revoke the API-issued key' >&2; exit 1; }
"${compose[@]}" run --rm --no-deps -T client disabled
if (( browser )); then
    "${compose[@]}" run --rm --no-deps -T client browser-ready
    retain=1
fi
printf '%s\n' 'OIDC smoke completed successfully'
