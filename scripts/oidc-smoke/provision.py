#!/usr/bin/env python3
"""Generate a disposable Keycloak realm and test credentials; never production data."""
import json
import os
import shutil
from pathlib import Path

root = Path(os.environ["SMOKE_DIR"])
credentials = {
    "user": "smoke-user",
    "password": os.environ["SMOKE_USER_PASSWORD"],
    "browser_user": "smoke-browser",
    "browser_password": os.environ["SMOKE_INTERACTIVE_PASSWORD"],
    "app_origin": os.environ["SMOKE_APP_ORIGIN"],
    "idp_origin": os.environ["SMOKE_IDP_ORIGIN"],
    "admin": "smoke-admin",
    "admin_password": os.environ["SMOKE_ADMIN_PASSWORD"],
    "api_secret": os.environ["SMOKE_API_SECRET"],
    "mcp_secret": os.environ["SMOKE_MCP_SECRET"],
    "short_secret": os.environ["SMOKE_SHORT_SECRET"],
    "wrong_secret": os.environ["SMOKE_WRONG_SECRET"],
    "collector_secret": os.environ["SMOKE_COLLECTOR_SECRET"],
}


def resource_client(client_id, secret, audience, introspection_client, lifetime=None):
    client = {
        "clientId": client_id,
        "enabled": True,
        "protocol": "openid-connect",
        "clientAuthenticatorType": "client-secret",
        "secret": secret,
        "publicClient": False,
        "standardFlowEnabled": False,
        "directAccessGrantsEnabled": True,
        "defaultClientScopes": ["basic"],
        "protocolMappers": [{
            "name": "resource-audience",
            "protocol": "openid-connect",
            "protocolMapper": "oidc-audience-mapper",
            "config": {
                "included.custom.audience": audience,
                "access.token.claim": "true",
                "id.token.claim": "false",
                "introspection.token.claim": "true",
            },
        }, {
            "name": "introspection-client-audience",
            "protocol": "openid-connect",
            "protocolMapper": "oidc-audience-mapper",
            "config": {
                "included.client.audience": introspection_client,
                "access.token.claim": "true",
                "id.token.claim": "false",
                "introspection.token.claim": "true",
            },
        }],
    }
    if lifetime is not None:
        client["attributes"] = {"access.token.lifespan": str(lifetime)}
    return client

realm = {
    "realm": "thalamus",
    "enabled": True,
    "sslRequired": "external",
    "accessTokenLifespan": 300,
    "registrationAllowed": False,
    "groups": [{"name": "smoke-admin"}],
    "clientScopes": [{
        # An explicit realm import suppresses Keycloak's built-in default
        # scopes. Restore the normal basic scope so access tokens and
        # introspection responses carry the real user subject.
        "name": "basic",
        "protocol": "openid-connect",
        "attributes": {
            "display.on.consent.screen": "false",
            "include.in.token.scope": "false",
        },
        "protocolMappers": [{
            "name": "sub",
            "protocol": "openid-connect",
            "protocolMapper": "oidc-sub-mapper",
            "config": {
                "access.token.claim": "true",
                "introspection.token.claim": "true",
            },
        }, {
            "name": "auth_time",
            "protocol": "openid-connect",
            "protocolMapper": "oidc-usersessionmodel-note-mapper",
            "config": {
                "user.session.note": "AUTH_TIME",
                "claim.name": "auth_time",
                "jsonType.label": "long",
                "access.token.claim": "true",
                "id.token.claim": "true",
                "introspection.token.claim": "true",
            },
        }],
    }, {
        "name": "groups",
        "protocol": "openid-connect",
        "protocolMappers": [{
            "name": "groups",
            "protocol": "openid-connect",
            "protocolMapper": "oidc-group-membership-mapper",
            "config": {
                "full.path": "true",
                "claim.name": "groups",
                "id.token.claim": "true",
                "access.token.claim": "true",
                "userinfo.token.claim": "true",
            },
        }],
    }],
    "clients": [
        {
            "clientId": "thalamus-browser",
            "enabled": True,
            "protocol": "openid-connect",
            "clientAuthenticatorType": "client-secret",
            "secret": os.environ["SMOKE_BROWSER_SECRET"],
            "publicClient": False,
            "standardFlowEnabled": True,
            "directAccessGrantsEnabled": False,
            "redirectUris": [credentials["app_origin"] + "/auth/callback"],
            "webOrigins": [credentials["app_origin"]],
            "defaultClientScopes": ["basic"],
            "optionalClientScopes": ["groups"],
        },
        {
            "clientId": "smoke-collector",
            "enabled": True,
            "protocol": "openid-connect",
            "clientAuthenticatorType": "client-secret",
            "secret": credentials["collector_secret"],
            "publicClient": False,
            "standardFlowEnabled": False,
            "directAccessGrantsEnabled": False,
            "serviceAccountsEnabled": True,
            "defaultClientScopes": ["basic"],
            "fullScopeAllowed": False,
            "protocolMappers": [{
                "name": "realm-management-client-roles",
                "protocol": "openid-connect",
                "protocolMapper": "oidc-usermodel-client-role-mapper",
                "config": {
                    "multivalued": "true",
                    "access.token.claim": "true",
                    "id.token.claim": "false",
                    "claim.name": "resource_access.${client_id}.roles",
                    "jsonType.label": "String",
                },
            }],
        },
        resource_client("api-client", credentials["api_secret"], credentials["app_origin"] + "/api", "api-client"),
        resource_client("mcp-client", credentials["mcp_secret"], credentials["app_origin"] + "/mcp", "mcp-client"),
        resource_client("short-client", credentials["short_secret"], credentials["app_origin"] + "/api", "api-client", 12),
        resource_client("wrong-client", credentials["wrong_secret"], credentials["app_origin"] + "/wrong", "api-client"),
    ],
    "users": [{
        "id": "54ac7e34-503a-446a-88b1-4884ea903114",
        "username": credentials["user"],
        "firstName": "Smoke",
        "lastName": "User",
        "email": "smoke-user@example.test",
        "requiredActions": [],
        "enabled": True,
        "emailVerified": True,
        "credentials": [{"type": "password", "value": credentials["password"], "temporary": False}],
        "groups": ["/smoke-admin"],
    }, {
        "id": "9b94a975-3491-41c3-8d0d-63fd8c0e6023",
        "username": credentials["browser_user"],
        "enabled": True,
        "firstName": "Smoke",
        "lastName": "Browser",
        "email": "smoke-browser@example.test",
        "requiredActions": [],
        "emailVerified": True,
        "credentials": [{"type": "password", "value": credentials["browser_password"], "temporary": False}],
        "groups": ["/smoke-admin"],
    }],
}
(root / "realm.json").write_text(json.dumps(realm), encoding="utf-8")
(root / "credentials.json").write_text(json.dumps(credentials), encoding="utf-8")
proxy = Path(__file__).with_name("nginx.conf").read_text(encoding="utf-8")
proxy = (proxy.replace("listen 443 ssl;", f"listen {os.environ['SMOKE_PORT']} ssl;")
         .replace("server_name idp.test;", f"server_name {os.environ['SMOKE_IDP_HOST']};")
         .replace("server_name app.test;", f"server_name {os.environ['SMOKE_APP_HOST']};")
         .replace("proxy_set_header X-Forwarded-Port 443;", "proxy_set_header X-Forwarded-Port $server_port;"))
(root / "nginx.conf").write_text(proxy, encoding="utf-8")

# Reuse the repository's complete illustrative catalog rather than fabricating
# deployment observations or omitting schema-required service configuration.
source = Path(__file__).resolve().parents[2] / "catalog"
catalog = root / "repo" / "catalog"
(catalog / "services").mkdir(parents=True)
(catalog / "limits").mkdir()
service = (source / "services" / "example-service.yaml").read_text(encoding="utf-8")
old_name = "name: example-service\n"
old_team = '"00000000-0000-4000-8000-000000000001"'
if service.count(old_name) != 1 or service.count(old_team) != 1:
    raise RuntimeError("example service fixture has changed")
service = service.replace(old_name, "name: smoke-service\n", 1).replace(
    old_team, '"3b3d3f25-f437-4f1a-bff5-36f6fac3714c"', 1
)
(catalog / "services" / "smoke.yaml").write_text(service, encoding="utf-8")
for name in ("artifacts.yaml", "hardware.yaml", "service.schema.json", "limits/example-shared.yaml"):
    shutil.copyfile(source / name, catalog / name)
