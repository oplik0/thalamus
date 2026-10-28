#!/usr/bin/env python3
"""Run inside the isolated compose network. This is a real TLS/OIDC client, not an IdP mock."""
import base64
import http.cookiejar
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path("/fixture")
CREDS = json.loads((ROOT / "credentials.json").read_text(encoding="utf-8"))
APP = CREDS["app_origin"]
IDP = CREDS["idp_origin"]
ISSUER = IDP + "/realms/thalamus"
TOKEN_URL = ISSUER + "/protocol/openid-connect/token"
INTROSPECT_URL = TOKEN_URL + "/introspect"
ADMIN = IDP + "/admin/realms/thalamus"
SUBJECT = "54ac7e34-503a-446a-88b1-4884ea903114"
PROJECTS = APP + "/api/v1/projects"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def request(method, url, *, headers=None, data=None, jar=None, trusted=True):
    context = ssl.create_default_context(cafile=str(ROOT / "ca.pem") if trusted else None)
    handlers = [urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context), NoRedirect()]
    if jar is not None:
        handlers.append(urllib.request.HTTPCookieProcessor(jar))
    opener = urllib.request.build_opener(*handlers)
    req = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        response = opener.open(req, timeout=20)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, response.headers, response.read()


def expect(description, response, status):
    actual, _, body = response
    if actual != status:
        raise AssertionError(f"{description}: expected HTTP {status}, received {actual}")
    return body

def oauth_token_response(description, response):
    status, _, body = response
    try:
        result = json.loads(body)
    except (UnicodeDecodeError, ValueError):
        raise AssertionError(f"{description}: HTTP {status}; invalid OAuth JSON") from None
    if not isinstance(result, dict):
        raise AssertionError(f"{description}: HTTP {status}; invalid OAuth JSON")
    if status != 200:
        # Never print the token response, provider-controlled arbitrary text, or
        # request credentials. Surface only standard codes and known static
        # Keycloak descriptions; redact anything else.
        codes = {
            "invalid_request", "invalid_client", "invalid_grant", "invalid_scope",
            "unauthorized_client", "unsupported_grant_type", "access_denied",
            "server_error", "temporarily_unavailable", "interaction_required",
        }
        descriptions = {
            "Account is not fully set up", "Invalid user credentials",
            "Invalid client or Invalid client credentials", "Client is not enabled",
            "Client not allowed for direct access grants",
            "Client not enabled to retrieve service account", "User account is disabled",
        }
        code = result.get("error")
        detail = result.get("error_description")
        code = code if isinstance(code, str) and code in codes else "<unrecognized>"
        detail = detail if isinstance(detail, str) and detail in descriptions else "<redacted>"
        raise AssertionError(f"{description}: HTTP {status}; OAuth error={code}; description={detail}")
    return result



def form(fields):
    return urllib.parse.urlencode(fields).encode("ascii")


def wait_for(url):
    deadline = time.monotonic() + 180
    last = None
    while time.monotonic() < deadline:
        try:
            response = request("GET", url)
            if response[0] == 200:
                return
            last = f"HTTP {response[0]}"
        except (OSError, urllib.error.URLError) as error:
            last = type(error).__name__
        time.sleep(2)
    raise RuntimeError(f"timed out waiting for {url} ({last})")


def direct_token(client_id, secret, username=None, password=None):
    response = request("POST", TOKEN_URL, data=form({
        "grant_type": "password", "client_id": client_id, "client_secret": secret,
        "username": username or CREDS["user"], "password": password or CREDS["password"], "scope": "openid",
    }), headers={"Content-Type": "application/x-www-form-urlencoded"})
    return oauth_token_response(f"token for {client_id}", response)


def introspect(token, client_id="api-client", secret=None):
    secret = secret or CREDS["api_secret"]
    basic = base64.b64encode(f"{client_id}:{secret}".encode()).decode()
    response = request("POST", INTROSPECT_URL, data=form({"token": token}), headers={
        "Content-Type": "application/x-www-form-urlencoded", "Authorization": f"Basic {basic}",
    })
    return json.loads(expect("Keycloak introspection", response, 200))

def diagnostic_claims(claims):
    return {name: claims.get(name) for name in ("active", "iss", "sub", "aud", "azp", "iat", "exp", "scope")}


def unverified_jwt_diagnostics(token):
    # Inspection only: authentication always uses Keycloak introspection.
    try:
        parts = token.split(".")
        if len(parts) != 3:
            return None
        payload = parts[1] + "=" * (-len(parts[1]) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload))
        return diagnostic_claims(claims) if isinstance(claims, dict) else None
    except (ValueError, UnicodeDecodeError):
        return None


def save_bearer_debug(state):
    if os.environ.get("SMOKE_RETAIN_FAILURE") != "1":
        return
    path = ROOT / "bearer-debug.json"
    path.write_text(json.dumps(state), encoding="utf-8")
    # The host-owned parent directory is 0700. Allow its owner to read files
    # written by the one-shot container's root user.
    path.chmod(0o644)


def assert_fixture_claims(label, claims):
    audience = claims.get("aud")
    if isinstance(audience, str):
        audience = [audience]
    if not isinstance(audience, list) or not all(isinstance(value, str) for value in audience):
        audience = []
    scope = claims.get("scope")
    scopes = scope.split(" ") if isinstance(scope, str) else []
    scope_valid = bool(scopes) and all(
        part and all(ord(ch) == 33 or 35 <= ord(ch) <= 91 or 93 <= ord(ch) <= 126 for ch in part)
        for part in scopes
    )
    expiry = claims.get("exp")
    checks = {
        "active": claims.get("active") is True,
        "issuer": claims.get("iss") == ISSUER,
        "subject": claims.get("sub") == SUBJECT,
        "resource_audience": APP + "/api" in audience,
        "client_audience": "api-client" in audience,
        "authorized_party": claims.get("azp") == "api-client",
        "scope_syntax": scope_valid,
        "openid_scope": "openid" in scopes,
        "future_expiry": type(expiry) is int and expiry > int(time.time()),
    }
    print(label + " introspection: " + " ".join(f"{name}={value}" for name, value in checks.items()))
    failures = [name for name, valid in checks.items() if not valid]
    if failures:
        raise AssertionError(label + " introspection failed: " + ", ".join(failures))


def bearer_response_error(response):
    try:
        body = json.loads(response[2])
        message = body.get("message") if isinstance(body, dict) else None
    except (UnicodeDecodeError, ValueError):
        message = None
    allowed = {"invalid credentials", "authentication required", "identity service unavailable"}
    return message if isinstance(message, str) and message in allowed else "<redacted>"


def bearer(token):
    return {"Authorization": "Bearer " + token}

def admin_token():
    response = request("POST", IDP + "/realms/master/protocol/openid-connect/token",
        data=form({"grant_type": "password", "client_id": "admin-cli",
                   "username": CREDS["admin"], "password": CREDS["admin_password"]}),
        headers={"Content-Type": "application/x-www-form-urlencoded"})
    return oauth_token_response("bootstrap admin token", response)["access_token"]


def admin_request(method, path, token, payload=None):
    headers = bearer(token)
    data = None
    if payload is not None:
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    return request(method, ADMIN + path, headers=headers, data=data)


def setup_collector():
    # Bootstrap admin is only used to assign two read-only roles to the
    # disposable service account, not by the collector itself.
    token = admin_token()
    client_ids = {}
    for client_id in ("smoke-collector", "realm-management"):
        path = "/clients?" + urllib.parse.urlencode({"clientId": client_id})
        clients = json.loads(expect("find " + client_id, admin_request("GET", path, token), 200))
        matching = [client["id"] for client in clients if client.get("clientId") == client_id]
        if len(matching) != 1:
            raise AssertionError("missing or ambiguous Keycloak client " + client_id)
        client_ids[client_id] = matching[0]
    collector_id = client_ids["smoke-collector"]
    management_id = client_ids["realm-management"]
    account = json.loads(expect("collector service account",
        admin_request("GET", f"/clients/{collector_id}/service-account-user", token), 200))
    roles = []
    for name in ("view-users", "view-realm"):
        roles.append(json.loads(expect("read role " + name,
            admin_request("GET", f"/clients/{management_id}/roles/{name}", token), 200)))
    expect("assign collector read-only roles", admin_request("POST",
        f"/users/{account['id']}/role-mappings/clients/{management_id}", token, roles), 204)
    # Restrict the client scope too; only these roles can enter its token.
    expect("scope collector read-only roles", admin_request("POST",
        f"/clients/{collector_id}/scope-mappings/clients/{management_id}", token, roles), 204)
    response = request("POST", TOKEN_URL, data=form({
        "grant_type": "client_credentials", "client_id": "smoke-collector",
        "client_secret": CREDS["collector_secret"],
    }), headers={"Content-Type": "application/x-www-form-urlencoded"})
    collector_token = oauth_token_response("collector client credentials", response)["access_token"]
    expect("collector users count", admin_request("GET", "/users/count", collector_token), 200)
    expect("collector user groups", admin_request("GET", f"/users/{SUBJECT}/groups", collector_token), 200)
    expect("collector user group count", admin_request("GET", f"/users/{SUBJECT}/groups/count", collector_token), 200)


class LoginForm(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.action = None

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "form" and attrs.get("id") == "kc-form-login":
            self.action = attrs.get("action")


def start_login(jar):
    response = request("GET", APP + "/auth/login", jar=jar)
    expect("start browser login", response, 302)
    location = response[1]["Location"]
    parsed = urllib.parse.urlsplit(location)
    if parsed.scheme != "https" or parsed.netloc != urllib.parse.urlsplit(IDP).netloc:
        raise AssertionError("authorization redirect is not the trusted IdP")
    state = urllib.parse.parse_qs(parsed.query).get("state", [])
    if len(state) != 1 or not state[0]:
        raise AssertionError("missing or ambiguous authorization state")
    return location, state[0]


def browser_login(username=None, password=None):
    jar = http.cookiejar.CookieJar()
    authorization, _ = start_login(jar)
    response = request("GET", authorization, jar=jar)
    page = expect("Keycloak authorization page", response, 200)
    parser = LoginForm()
    parser.feed(page.decode("utf-8"))
    if not parser.action:
        raise AssertionError("Keycloak login form missing")
    action = urllib.parse.urljoin(authorization, parser.action)
    parsed = urllib.parse.urlsplit(action)
    if parsed.scheme != "https" or parsed.netloc != urllib.parse.urlsplit(IDP).netloc:
        raise AssertionError("login form action is not the trusted IdP")
    response = request("POST", action, jar=jar, data=form({
        "username": username or CREDS["user"], "password": password or CREDS["password"], "credentialId": "",
    }), headers={"Content-Type": "application/x-www-form-urlencoded", "Origin": IDP})
    expect("Keycloak browser login", response, 302)
    callback = response[1]["Location"]
    parsed = urllib.parse.urlsplit(callback)
    if parsed.scheme != "https" or parsed.netloc != urllib.parse.urlsplit(APP).netloc or parsed.path != "/auth/callback":
        raise AssertionError("unexpected OIDC callback destination")
    response = request("GET", callback, jar=jar)
    expect("OIDC callback", response, 303)
    sessions = [c.value for c in jar if c.name == "__Host-thalamus-session" and c.domain == urllib.parse.urlsplit(APP).hostname]
    if len(sessions) != 1:
        raise AssertionError("browser session cookie missing or ambiguous")
    return "__Host-thalamus-session=" + sessions[0]


def create_project(headers, name):
    body = json.dumps({"team": "smoke-team", "name": name, "budget": None}).encode()
    return request("POST", PROJECTS, headers={**headers, "Content-Type": "application/json"}, data=body)

def create_key(headers, service="smoke-service"):
    body = json.dumps({
        "name": "smoke-key", "kind": "personal",
        "team_id": "3b3d3f25-f437-4f1a-bff5-36f6fac3714c",
        "project_id": None, "allowed_services": [service], "expires_at": None,
    }).encode()
    return request("POST", APP + "/api/v1/keys",
        headers={**headers, "Content-Type": "application/json"}, data=body)


def mcp_initialize(token):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-03-26", "capabilities": {},
        "clientInfo": {"name": "thalamus-smoke", "version": "1"},
    }}).encode()
    return request("POST", APP + "/mcp", data=body, headers={
        **bearer(token), "Content-Type": "application/json", "Accept": "application/json, text/event-stream",
    })


def run_tests():
    # An IdP token must not provision a local user: a real browser login does that.
    api = direct_token("api-client", CREDS["api_secret"])["access_token"]
    before = introspect(api)
    debug_state = {
        "api_token": api,
        "jwt_unverified_for_diagnostics": unverified_jwt_diagnostics(api),
        "introspection_before_login": diagnostic_claims(before),
    }
    save_bearer_debug(debug_state)
    assert_fixture_claims("before browser login", before)
    expect("unprovisioned bearer", request("GET", PROJECTS, headers=bearer(api)), 401)
    expect("missing credentials", request("GET", PROJECTS), 401)

    # Reject a callback carrying another browser's state before any token exchange.
    other_jar = http.cookiejar.CookieJar()
    _, state = start_login(other_jar)
    bad_callback = APP + "/auth/callback?" + urllib.parse.urlencode({"state": state + "x", "code": "unused"})
    expect("cross-browser callback", request("GET", bad_callback, jar=other_jar), 401)
    cookie = browser_login()
    debug_state["browser_cookie"] = cookie
    save_bearer_debug(debug_state)
    cookie_headers = {"Cookie": cookie}
    expect("browser session", request("GET", PROJECTS, headers=cookie_headers), 200)
    after = introspect(api)
    debug_state["introspection_after_login"] = diagnostic_claims(after)
    save_bearer_debug(debug_state)
    assert_fixture_claims("after browser login", after)
    response = request("GET", PROJECTS, headers=bearer(api))
    if response[0] != 200:
        category = bearer_response_error(response)
        debug_state["api_response"] = {"status": response[0], "category": category}
        save_bearer_debug(debug_state)
        raise AssertionError(f"valid API bearer: HTTP {response[0]}; category={category}")
    expect("cookie and bearer ambiguity", request("GET", PROJECTS, headers={**cookie_headers, **bearer(api)}), 401)
    expect("duplicate session cookies", request("GET", PROJECTS, headers={"Cookie": cookie + "; " + cookie}), 401)
    services = json.loads(expect("published service catalog",
        request("GET", APP + "/api/v1/services", headers=cookie_headers), 200))
    if [service["name"] for service in services["services"]] != ["smoke-service"]:
        raise AssertionError("fixture service not visible through committed catalog policy")


    expect("cookie write without Origin", create_project(cookie_headers, "rejected-no-origin"), 403)
    expect("cookie write with foreign Origin", create_project({**cookie_headers, "Origin": "https://evil.test"}, "rejected-foreign"), 403)
    expect("cookie write with valid Origin", create_project({**cookie_headers, "Origin": APP}, "browser-project"), 201)
    expect("bearer write without Origin", create_project(bearer(api), "bearer-project"), 201)
    listed = json.loads(expect("list created projects", request("GET", PROJECTS, headers=cookie_headers), 200))
    names = {project["name"] for project in listed["projects"]}
    if names != {"browser-project", "bearer-project"}:
        raise AssertionError("unexpected project set after CSRF and bearer checks")
    expect("service outside committed access policy", create_key(bearer(api), "not-authorized"), 404)
    issued = json.loads(expect("create key under committed access policy", create_key(bearer(api)), 201))
    if issued["key"]["kind"] != "personal" or not issued["secret"].startswith("thl_"):
        raise AssertionError("invalid personal key response")

    wrong = direct_token("wrong-client", CREDS["wrong_secret"])["access_token"]
    wrong_claims = introspect(wrong)
    if not wrong_claims.get("active") or APP + "/api" in wrong_claims.get("aud", []):
        raise AssertionError("wrong-audience token was not an active distinct resource token")
    expect("wrong API audience", request("GET", PROJECTS, headers=bearer(wrong)), 401)
    mcp = direct_token("mcp-client", CREDS["mcp_secret"])["access_token"]
    expect("API token rejected by MCP", mcp_initialize(api), 401)
    expect("MCP token rejected by API", request("GET", PROJECTS, headers=bearer(mcp)), 401)
    mcp_response = mcp_initialize(mcp)
    mcp_body = expect("valid MCP initialize", mcp_response, 200)
    if b'"result"' not in mcp_body:
        raise AssertionError("MCP did not complete initialization")

    short_response = direct_token("short-client", CREDS["short_secret"])
    short = short_response["access_token"]
    lifetime = short_response["expires_in"]
    if not 0 < lifetime <= 20:
        raise AssertionError(f"short-lived token lifetime not applied: {lifetime}")
    expect("short-lived token before expiry", request("GET", PROJECTS, headers=bearer(short)), 200)
    time.sleep(lifetime + 2)
    expect("expired API bearer", request("GET", PROJECTS, headers=bearer(short)), 401)

    # Fresh credentials are retained for the subsequent offboarding phases.
    api = direct_token("api-client", CREDS["api_secret"])["access_token"]
    mcp = direct_token("mcp-client", CREDS["mcp_secret"])["access_token"]
    (ROOT / "state.json").write_text(json.dumps({"api": api, "mcp": mcp, "cookie": cookie}), encoding="utf-8")
    print("Passed: real browser login, local provisioning, CSRF, API/MCP bearer boundaries, expiry and ambiguity")


def disable_at_keycloak():
    state = json.loads((ROOT / "state.json").read_text(encoding="utf-8"))
    token = admin_token()
    user = json.loads(expect("read fixture user",
        admin_request("GET", f"/users/{SUBJECT}", token), 200))
    if user.get("id") != SUBJECT or user.get("enabled") is not True:
        raise AssertionError("fixture user was not enabled before offboarding")
    user["enabled"] = False
    expect("disable user at Keycloak",
        admin_request("PUT", f"/users/{SUBJECT}", token, user), 204)
    expect("browser session before collector sync",
        request("GET", PROJECTS, headers={"Cookie": state["cookie"]}), 200)
    print("Keycloak user disabled; local browser session remains active until sync")


def check_disabled():
    state = json.loads((ROOT / "state.json").read_text(encoding="utf-8"))
    # The identity integration test has run the real collector/sync between
    # this check and disable_at_keycloak, not a direct database update.
    expect("disabled local user, API", request("GET", PROJECTS, headers=bearer(state["api"])), 401)
    expect("disabled local user, bearer mutation", create_project(bearer(state["api"]), "disabled"), 401)
    expect("disabled local user, browser", request("GET", PROJECTS, headers={"Cookie": state["cookie"]}), 401)
    expect("disabled local user, MCP", mcp_initialize(state["mcp"]), 401)
    print("Passed: collector offboarding revoked the local session and access")


def prepare_browser():
    cookie = browser_login(CREDS["browser_user"], CREDS["browser_password"])
    expect("active browser user after collector sync",
        request("GET", PROJECTS, headers={"Cookie": cookie}), 200)
    token = direct_token("api-client", CREDS["api_secret"],
        CREDS["browser_user"], CREDS["browser_password"])
    expect("active browser user API bearer",
        request("GET", PROJECTS, headers=bearer(token["access_token"])), 200)
    browser_file = ROOT / "browser.json"
    browser_file.write_text(json.dumps({
        "origin": APP, "idp_origin": IDP, "ca_file": "ca.pem",
        "username": CREDS["browser_user"], "password": CREDS["browser_password"],
        "cookie": cookie,
        "api_token": token["access_token"], "api_token_expires_in": token["expires_in"],
    }), encoding="utf-8")
    # The parent SMOKE_DIR is 0700; the host user must be able to read this
    # when the one-shot client container runs as root.
    browser_file.chmod(0o644)
    print("Separate active browser user provisioned")


def main():
    phase = sys.argv[1]
    if phase == "wait-idp":
        discovery = ISSUER + "/.well-known/openid-configuration"
        wait_for(discovery)
        try:
            request("GET", discovery, trusted=False)
        except urllib.error.URLError as error:
            if not isinstance(error.reason, ssl.SSLCertVerificationError):
                raise
        else:
            raise AssertionError("private IdP unexpectedly trusted without its CA")
        setup_collector()
        print("Keycloak available with private CA; default trust rejects it")
    elif phase == "wait-app":
        wait_for(APP + "/health")
        print("Thalamus available through HTTPS proxy")
    elif phase == "run":
        run_tests()
    elif phase == "disable-idp":
        disable_at_keycloak()
    elif phase == "disabled":
        check_disabled()
    elif phase == "browser-ready":
        prepare_browser()
    else:
        raise ValueError("unknown phase")


if __name__ == "__main__":
    main()
