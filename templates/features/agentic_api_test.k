# Run from the repository root: kcl test ./templates/features
import runtime

_test_recipe = {
    name: "model"
    namespace: "models"
    mode: "shared-stateless"
    image: "registry.example/agentic@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    upstreamCommit: "27e60cf006cb771cf1961ea9909e64d812f5f3c8"
    gatewayVersion: "v1.7.0"
    publicGateway: "public"
    publicGatewayNamespace: "gateways"
    internalGateway: "internal"
    internalGatewayNamespace: "gateways"
    internalGatewayService: "internal-proxy"
    internalHostname: "internal-proxy.gateways.svc.cluster.local"
    internalBackend: "model-responses"
    serviceKeySecret: "service-key"
    gatewayNamespaceLabels: {"kubernetes.io/metadata.name": "gateways"}
    gatewayPodLabels: {app: "public-proxy"}
    instances: [{name: "shared", hostname: "model.example.test", databaseSecret: "db", keySelector: {service: "model"}}]
}

test_stateless_route_and_policy = lambda {
    r = Recipe {**_test_recipe}
    policy = r.items[3]
    assert policy.spec.traffic.apiKeyAuthentication.mode == "Strict"
    assert "b.store == false" in policy.spec.traffic.authorization.policy.matchExpressions[1]
    assert '"previous_response_id"' not in policy.spec.traffic.authorization.policy.matchExpressions[1]
    assert r.items[2].spec.rules[0].matches[0].method == "POST"
    assert r.items[2].spec.rules[0].matches[0].path.value == "/v1/responses"
    assert r.items[4].spec.policies.auth.secretRef.name == "service-key"
    assert r.items[8].spec.policyTypes == ["Ingress"]
}

test_invalid_image_rejected = lambda {
    err = runtime.catch(lambda {
        r = Recipe {**_test_recipe, image = "registry.example/agentic:latest"}
    })
    assert err != ""
}

test_team_databases_must_be_distinct = lambda {
    err = runtime.catch(lambda {
        r = Recipe {
            **_test_recipe
            mode = "team-shared"
            instances = [
                {name: "a", team: "a", hostname: "a.example.test", databaseSecret: "same", keySelector: {team: "a"}},
                {name: "b", team: "b", hostname: "b.example.test", databaseSecret: "same", keySelector: {team: "b"}},
            ]
        }
    })
    assert err == "each instance requires a separate database Secret"
}

test_team_policy_binds_authenticated_team = lambda {
    r = Recipe {
        **_test_recipe
        mode = "team-shared"
        instances = [{name: "a", team: "a", hostname: "a.example.test", databaseSecret: "db-a", keySelector: {team: "a"}}]
    }
    expressions = r.items[3].spec.traffic.authorization.policy.matchExpressions
    assert 'apiKey.team == "a"' in expressions[0]
    assert '"previous_response_id"' in expressions[1]
}
