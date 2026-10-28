# Run from the repository root: kcl test ./templates/gateway
# These tests assert rendered policy contracts; they do not execute CEL or
# substitute for a live controller, gateway, Envoy RLS and Redis integration test.
import runtime
import yaml

_rate_base = {
    name: "rate-limit"
    namespace: "inference"
    port: 8081
    domain: "example"
    teamCaps: {"44444444-4444-4444-8444-444444444444": 1000}
    teamWindows: {"44444444-4444-4444-8444-444444444444": "day"}
}
_test_rate = RateLimit {**_rate_base}
_test_selector = {"thalamus.dev/gateway-keys": "true"}
_policy_base = {
    namespace: "inference"
    service: "chat"
    route: "chat"
    keySelector: _test_selector
    version: "v1.7.0"
    rateLimit: _test_rate
}
_candidate_base = {**_policy_base, route = "model-candidate", candidateDeployment = "model"}
_benchmark_base = {**_policy_base, route = "chat-benchmark", benchmark = True}
_cap_base = {team_id: "11111111-1111-4111-8111-111111111111", max_tokens: 100, unit: "tokens", window: "day"}
_profile_base = {name: "shared", service: "rl", namespace: "gateways", port: 8081, domain: "cluster"}
_baseline = Policy {**_policy_base}
_public = Policy {**_policy_base, trafficClasses = ["standard", "batch"]}
_candidate = Policy {**_candidate_base}
_benchmark = Policy {**_benchmark_base}

test_public_resource_auth_and_scope = lambda {
    p = _public.item
    req = p.spec.traffic.authorization.policy.matchExpressions
    assert p.spec.targetRefs == [{group: "gateway.networking.k8s.io", kind: "HTTPRoute", name: "chat"}]
    assert p.spec.traffic.apiKeyAuthentication.mode == "Strict"
    assert p.spec.traffic.apiKeyAuthentication.configMapSelector.matchLabels == _test_selector
    assert p.spec.traffic.authorization.action == "Require"
    assert any e in req {'apiKey.key_id.matches(' in e}
    assert any e in req {'apiKey.user.matches(' in e}
    assert any e in req {'apiKey.creator.matches(' in e}
    assert any e in req {'apiKey.team.matches(' in e and 'apiKey.team in ' in e}
    assert any e in req {'apiKey.project == null && apiKey.kind == "personal"' in e and 'apiKey.project.matches(' in e}
    assert any e in req {'"chat" in apiKey.allowed_services' in e}
    assert any e in req {'request.startTime < timestamp(apiKey.expiry)' in e}
    assert any e in req {'apiKey.kind in ["personal", "project", "job", "debug"]' in e}
    assert any e in req {'apiKey.kind != "job"' in e and 'apiKey.run_id.matches(' in e and 'apiKey.deployment != ""' in e}
    assert 'apiKey.kind != "job" || apiKey.run_kind == "validation"' in req
    assert any e in req {'apiKey.kind != "debug"' in e and 'apiKey.debug_session_id.matches(' in e and 'apiKey.debug_model == "chat"' in e and 'json(request.body).model == apiKey.debug_model' in e and 'request.method != "POST"' in e}
    assert any e in req {'apiKey.team_budget ==' in e and 'apiKey.budget_window ==' in e and '"day"' in e}
    assert any e in req {'apiKey.class in ' in e and '"standard"' in e and '"batch"' in e}
}

test_candidate_is_only_bound_job = lambda {
    p = _candidate.item
    req = p.spec.traffic.authorization.policy.matchExpressions
    assert p.spec.targetRefs[0].name == "model-candidate"
    assert any e in req {e == 'apiKey.kind == "job" && apiKey.deployment == "model"'}
    assert any e in req {'apiKey.run_id.matches(' in e and 'apiKey.expiry != null' in e and 'apiKey.budget != null' in e}
    # Unlike candidate policy, the public policy does not impose the job-only rule.
    assert not (any e in _public.item.spec.traffic.authorization.policy.matchExpressions {e == 'apiKey.kind == "job" && apiKey.deployment == "model"'})
}

test_global_token_budgets_and_server_config = lambda {
    g = _public.item.spec.traffic.rateLimit["global"]
    assert g.failureMode == "FailClosed"
    assert len(g.descriptors) == 3
    assert [d.entries[0].name for d in g.descriptors] == ["key_id", "project", "team"]
    assert all d in g.descriptors {d.unit == "Tokens" and '"day"' in d.limitOverride and '[apiKey.team]' in d.limitOverride}
    assert g.descriptors[0].entries[0].expression == "apiKey.key_id"
    assert 'apiKey.budget == null' in g.descriptors[0].limitOverride
    assert 'apiKey.budget >' in g.descriptors[0].limitOverride
    assert 'apiKey.project_budget == null' in g.descriptors[1].limitOverride
    assert g.descriptors[1].entries[0].expression == "apiKey.project"
    assert '44444444-4444-4444-8444-444444444444' in g.descriptors[2].limitOverride
    cm = limiter_config("inference", _test_rate)
    config = yaml.decode(cm.data["config.yaml"])
    assert config.domain == g.domain
    assert [d.key for d in config.descriptors] == ["key_id", "project", "team", "benchmark_key"]
    assert all d in config.descriptors {d.rate_limit.unit == "day" and d.rate_limit.requests_per_unit == 1}
}

test_metadata_requests_debit_shared_budget_counters = lambda {
    token_policy = _baseline.item
    request_policy = Policy {**_policy_base, requestRateLimit = True}.item
    token_limit = token_policy.spec.traffic.rateLimit["global"]
    request_limit = request_policy.spec.traffic.rateLimit["global"]
    assert request_limit.failureMode == "FailClosed"
    assert request_limit.domain == token_limit.domain and request_limit.backendRef == token_limit.backendRef
    assert [d.entries for d in request_limit.descriptors] == [d.entries for d in token_limit.descriptors]
    assert [d.limitOverride for d in request_limit.descriptors] == [d.limitOverride for d in token_limit.descriptors]
    assert [d.entries[0].name for d in request_limit.descriptors] == ["key_id", "project", "team"]
    assert all d in request_limit.descriptors {d.unit == "Requests" and "cost" not in d}
    assert all d in token_limit.descriptors {d.unit == "Tokens"}
    assert request_policy.spec.traffic.authorization == token_policy.spec.traffic.authorization
}

test_metadata_request_limit_cannot_be_disabled_or_combined = lambda {
    request_base = {**_policy_base, requestRateLimit = True}
    assert runtime.catch(lambda {p = Policy {**request_base}}) == Undefined
    assert runtime.catch(lambda {p = Policy {**_benchmark_base}}) == Undefined
    no_limit = runtime.catch(lambda {p = Policy {**request_base, applyRateLimit = False}})
    forwarding = runtime.catch(lambda {p = Policy {**request_base, forwardBearer = True}})
    benchmark = runtime.catch(lambda {p = Policy {**_benchmark_base, requestRateLimit = True}})
    assert no_limit != Undefined and forwarding != Undefined and benchmark != Undefined
}

test_caller_headers_are_replaced_or_rejected = lambda {
    traffic = _public.item.spec.traffic
    req = traffic.authorization.policy.matchExpressions
    xform = traffic.transformation.request
    assert any e in req {'request.headers.all(' in e and 'x-thalamus-' in e and 'x-llm-d-inference-' in e}
    assert "x-api-key" in xform.remove and "x-thalamus-backend" in xform.remove
    assert xform.metadata == {thalamus_service: '"chat"'}
    assert {s.name: s.value for s in xform.set} == {
        "x-thalamus-user": "apiKey.user"
        "x-thalamus-project": "apiKey.project"
        "x-thalamus-team": "apiKey.team"
        "x-thalamus-class": "apiKey.class"
        "x-llm-d-inference-objective": '"chat-" + apiKey.class'
        "x-llm-d-inference-fairness-id": 'apiKey.project == null ? apiKey.team : apiKey.project'
    }
    disabled = _candidate.item.spec.traffic.transformation.request
    assert "x-thalamus-class" in disabled.remove
    assert "x-llm-d-inference-objective" in disabled.remove
    assert "x-llm-d-inference-fairness-id" in disabled.remove
}

test_agentic_boundary_strips_untrusted_identity = lambda {
    p = Policy {
        **_policy_base
        route = "chat-responses"
        forwardBearer = True
        applyRateLimit = False
    }.item
    assert "rateLimit" not in p.spec.traffic
    assert p.spec.traffic.transformation.request.set == [{name: "authorization", value: '"Bearer " + apiKey.key.unredacted()'}]
    assert "x-thalamus-user" in p.spec.traffic.transformation.request.remove
    assert "x-llm-d-inference-fairness-id" in p.spec.traffic.transformation.request.remove
}

test_wrong_selector_is_rejected = lambda {
    assert _baseline.item.spec.traffic.apiKeyAuthentication.mode == "Strict"
    err = runtime.catch(lambda {
        p = Policy {**_policy_base, keySelector = {wrong: "true"}}
    })
    assert err != Undefined
}

test_wrong_gateway_version_is_rejected = lambda {
    assert _baseline.item.spec.traffic.apiKeyAuthentication.mode == "Strict"
    err = runtime.catch(lambda {
        p = Policy {**_policy_base, version = "v1.6.0"}
    })
    assert err != Undefined
}

test_wrong_candidate_route_is_rejected = lambda {
    assert _candidate.item.spec.targetRefs[0].name == "model-candidate"
    err = runtime.catch(lambda {
        p = Policy {**_candidate_base, route = "other-candidate"}
    })
    assert err != Undefined
}

test_rate_limits_cannot_be_disabled_for_normal_route = lambda {
    assert _baseline.item.spec.traffic.rateLimit["global"].failureMode == "FailClosed"
    err = runtime.catch(lambda {
        p = Policy {**_policy_base, applyRateLimit = False}
    })
    assert err != Undefined
}

test_bad_budget_configuration_is_rejected = lambda {
    assert _test_rate.teamCaps["44444444-4444-4444-8444-444444444444"] > 0
    err = runtime.catch(lambda {
        r = RateLimit {**_rate_base, teamCaps = {"44444444-4444-4444-8444-444444444444": 4294967296}}
    })
    assert err != Undefined
}

test_unknown_budget_window_is_rejected = lambda {
    assert _test_rate.teamWindows["44444444-4444-4444-8444-444444444444"] == "day"
    err = runtime.catch(lambda {
        r = RateLimit {**_rate_base, teamWindows = {"44444444-4444-4444-8444-444444444444": "week"}}
    })
    assert err != Undefined
}

test_limiter_config_namespace_mismatch_is_rejected = lambda {
    assert yaml.decode(limiter_config("inference", _test_rate).data["config.yaml"]).domain == _test_rate.domain
    err = runtime.catch(lambda {cm = limiter_config("other", _test_rate)})
    assert err != Undefined
}

test_traffic_classes_must_be_unique_and_fit_scoped_objective = lambda {
    assert "x-llm-d-inference-objective" in [s.name for s in _public.item.spec.traffic.transformation.request.set]
    err = runtime.catch(lambda {
        p = Policy {**_policy_base, trafficClasses = ["standard", "standard"]}
    })
    assert err != Undefined
}

test_month_window_and_run_binding = lambda {
    rate = RateLimit {name: "rl", namespace: "inference", port: 8081, domain: "shared", teamCaps: {"44444444-4444-4444-8444-444444444444": 12}, teamWindows: {"44444444-4444-4444-8444-444444444444": "month"}}
    p = Policy {namespace: "inference", service: "chat", route: "model-candidate", candidateDeployment: "model", keySelector: _test_selector, version: "v1.7.0", rateLimit: rate}
    descriptors = p.item.spec.traffic.rateLimit["global"].descriptors
    assert all d in descriptors {'"month"' in d.limitOverride and '[apiKey.team]' in d.limitOverride}
    assert any e in p.item.spec.traffic.authorization.policy.matchExpressions {'apiKey.budget_window ==' in e and '"month"' in e and 'apiKey.team_budget ==' in e}
    config = yaml.decode(limiter_config("inference", rate).data["config.yaml"])
    assert all d in config.descriptors {d.rate_limit.unit == "day"}
}

test_no_forwarding_candidate_credentials = lambda {
    assert _candidate.item.spec.targetRefs[0].name == "model-candidate"
    err = runtime.catch(lambda {
        p = Policy {**_candidate_base, forwardBearer = True}
    })
    assert err != Undefined
}

test_empty_caps_admit_no_teams_and_malformed_team_rejected = lambda {
    empty = RateLimit {**_rate_base, teamCaps = {}, teamWindows = {}}
    p = Policy {**_policy_base, rateLimit = empty}
    assert any e in p.item.spec.traffic.authorization.policy.matchExpressions {'apiKey.team in {}' in e}
    assert _test_rate.teamCaps["44444444-4444-4444-8444-444444444444"] > 0
    malformed = runtime.catch(lambda {
        r = RateLimit {**_rate_base, teamCaps = {"not-a-uuid": 100}, teamWindows = {"not-a-uuid": "day"}}
    })
    assert malformed != Undefined
}

test_authorization_expression_count_is_bounded = lambda {
    assert _baseline.item.spec.traffic.authorization.action == "Require"
    err = runtime.catch(lambda {
        p = Policy {**_policy_base, extraRequirements = ["true" for _ in range(256)]}
    })
    assert err != Undefined
}

test_legacy_benchmark_is_job_only_and_counts_requests = lambda {
    p = Policy {
        namespace: "inference", service: "chat", route: "chat-benchmark"
        keySelector: _test_selector, version: "v1.7.0", rateLimit: _test_rate
        benchmark: True
    }.item
    req = p.spec.traffic.authorization.policy.matchExpressions
    global_limit = p.spec.traffic.rateLimit["global"]
    assert any e in req {'apiKey.kind == "job"' in e and 'apiKey.run_kind == "benchmark"' in e and 'apiKey.request_budget > 0' in e and 'apiKey.request_budget <= 1000000' in e}
    assert any e in req {'apiKey.budget >= 0' in e}
    assert any e in req {'apiKey.run_id.matches(' in e and 'apiKey.expiry != null' in e and 'apiKey.budget != null' in e}
    assert global_limit.failureMode == "FailClosed"
    assert len(global_limit.descriptors) == 1
    assert global_limit.descriptors[0].unit == "Requests"
    assert global_limit.descriptors[0].entries == [{name: "benchmark_key", expression: "apiKey.key_id"}]
    assert 'apiKey.request_budget' in global_limit.descriptors[0].limitOverride
    assert 'apiKey.budget' not in global_limit.descriptors[0].limitOverride
}

test_candidate_benchmark_is_bound_to_deployment = lambda {
    p = Policy {
        namespace: "inference", service: "chat", route: "model-benchmark"
        candidateDeployment: "model", benchmark: True
        keySelector: _test_selector, version: "v1.7.0", rateLimit: _test_rate
    }.item
    assert p.spec.targetRefs[0].name == "model-benchmark"
    assert 'apiKey.kind == "job" && apiKey.deployment == "model"' in p.spec.traffic.authorization.policy.matchExpressions
    assert any e in p.spec.traffic.authorization.policy.matchExpressions {'apiKey.run_kind == "benchmark"' in e and 'apiKey.request_budget > 0' in e}
    assert p.spec.traffic.rateLimit["global"].descriptors[0].unit == "Requests"
}

test_benchmark_route_and_rate_limit_cannot_be_bypassed = lambda {
    assert _benchmark.item.spec.traffic.rateLimit["global"].descriptors[0].unit == "Requests"
    bad_route = runtime.catch(lambda {
        p = Policy {**_benchmark_base, route = "chat"}
    })
    no_limit = runtime.catch(lambda {
        p = Policy {**_benchmark_base, applyRateLimit = False}
    })
    assert bad_route != Undefined
    assert no_limit != Undefined
}

test_central_metrics_policy_uses_bounded_verified_dimensions = lambda {
    p = metrics_policy("gateway-system", "shared-gateway")
    assert p.spec.targetRefs == [{group: "gateway.networking.k8s.io", kind: "Gateway", name: "shared-gateway"}]
    attrs = p.spec.frontend.metrics.attributes.add
    assert {a.name: a.expression for a in attrs} == {
        team_id: 'coalesce(apiKey.team, "")'
        project_id: 'coalesce(apiKey.project, "")'
        service: 'coalesce(metadata.thalamus_service, "")'
        traffic_class: 'coalesce(apiKey.class, "")'
    }
    assert "key_id" not in [a.name for a in attrs]
    assert "user_id" not in [a.name for a in attrs]
}

test_metrics_gateway_identity_is_validated = lambda {
    assert metrics_policy("gateway-system", "shared-gateway").spec.targetRefs[0].name == "shared-gateway"
    err = runtime.catch(lambda {p = metrics_policy("gateway-system", "INVALID_NAME")})
    assert err != Undefined
}

test_valid_long_gateway_and_non_dns_domain_use_safe_names = lambda {
    p = metrics_policy("gateway-system", "a-very-long-gateway-name-that-needs-a-safe-object-name-12345678")
    assert p.metadata.name.startswith("gateway-usage-")
    assert len(p.metadata.name) <= 63
    r = RateLimit {name: "rl", namespace: "inference", port: 8081, domain: "team A/prod", teamCaps: {"44444444-4444-4444-8444-444444444444": 1000}, teamWindows: {"44444444-4444-4444-8444-444444444444": "day"}}
    cm = limiter_config("inference", r)
    assert cm.metadata.name.startswith("gateway-ratelimit-")
}

test_catalog_profile_resolves_each_team_and_excludes_zero = lambda {
    profile = LimitProfile {name: "shared", service: "rl", namespace: "gateways", port: 8081, domain: "cluster"}
    caps = BudgetCaps {teams: [
        {team_id: "11111111-1111-4111-8111-111111111111", max_tokens: 100, unit: "tokens", window: "day", allowed_classes: ["standard"]},
        {team_id: "22222222-2222-4222-8222-222222222222", max_tokens: 200, unit: "tokens", window: "month", allowed_classes: ["standard", "batch"]},
        {team_id: "33333333-3333-4333-8333-333333333333", max_tokens: 0, unit: "tokens", window: "day"},
    ]}
    rate = from_catalog(profile, caps)
    assert rate.teamCaps == {"11111111-1111-4111-8111-111111111111": 100, "22222222-2222-4222-8222-222222222222": 200}
    assert rate.teamWindows == {"11111111-1111-4111-8111-111111111111": "day", "22222222-2222-4222-8222-222222222222": "month"}
    p = Policy {namespace: "inference", service: "chat", route: "chat", keySelector: _test_selector, version: "v1.7.0", rateLimit: rate}
    assert all d in p.item.spec.traffic.rateLimit["global"].descriptors {'"day"' in d.limitOverride and '"month"' in d.limitOverride}
    assert "33333333-3333-4333-8333-333333333333" not in rate.teamCaps
}

test_catalog_rejects_duplicate_oversized_and_invalid_entries = lambda {
    profile = LimitProfile {**_profile_base}
    caps = BudgetCaps {teams: [_cap_base]}
    assert from_catalog(profile, caps).teamCaps[_cap_base.team_id] > 0
    duplicate = runtime.catch(lambda {
        caps = BudgetCaps {teams: [_cap_base, _cap_base]}
    })
    too_large = runtime.catch(lambda {
        cap = BudgetCap {**_cap_base, max_tokens = 4294967296}
    })
    bad_name = runtime.catch(lambda {
        profile = LimitProfile {**_profile_base, name = "../bad"}
    })
    assert duplicate != Undefined
    assert too_large != Undefined
    assert bad_name != Undefined
}
