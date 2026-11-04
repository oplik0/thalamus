package main

import (
	"encoding/json"
	"fmt"
	"regexp"
	"sort"
	"strings"

	celgo "github.com/google/cel-go/cel"
	admissionv1 "k8s.io/api/admissionregistration/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/util/version"
	admissioncel "k8s.io/apiserver/pkg/admission/plugin/cel"
	apicel "k8s.io/apiserver/pkg/cel"
	"k8s.io/apiserver/pkg/cel/common"
	"k8s.io/apiserver/pkg/cel/environment"
	"k8s.io/apiserver/pkg/cel/library"
	"k8s.io/apiserver/pkg/cel/openapi"
	"k8s.io/apiserver/pkg/cel/openapi/resolver"
	"k8s.io/kube-openapi/pkg/validation/spec"
)

// Keep offline work bounded. Wildcard and non-status subresource rules cannot
// be exhaustively typed from this local bundle and are rejected explicitly.
const (
	maxPolicyKinds                = 10
	maxPolicyRules                = 64
	maxPolicyRuleCombinations     = 256
	maxPolicyExpressions          = 256
	maxPolicyExpressionBytes      = 64 * 1024
	maxPolicyTotalExpressionBytes = 1024 * 1024
)

var celIdentifier = regexp.MustCompile(`^[_a-zA-Z][_a-zA-Z0-9]*$`)
var celReserved = map[string]bool{
	"true": true, "false": true, "null": true, "in": true, "as": true,
	"break": true, "const": true, "continue": true, "else": true, "for": true,
	"function": true, "if": true, "import": true, "let": true, "loop": true,
	"package": true, "namespace": true, "return": true, "var": true, "void": true, "while": true,
}

type admissionExpression struct {
	text    string
	returns []*celgo.Type
}

func (e admissionExpression) GetExpression() string      { return e.text }
func (e admissionExpression) ReturnTypes() []*celgo.Type { return e.returns }

type admissionVariable struct{ name, expression string }

func (v admissionVariable) GetName() string       { return v.name }
func (v admissionVariable) GetExpression() string { return v.expression }
func (v admissionVariable) ReturnTypes() []*celgo.Type {
	return []*celgo.Type{celgo.AnyType, celgo.DynType}
}

type labeledExpression struct {
	path       string
	text       string
	returns    []*celgo.Type
	authorizer bool
}

// The Kubernetes reference resolver intentionally replaces recursive schemas
// with an empty object. Detect cycles first so that this gate never silently
// reports a partial CEL type check as complete.
func (e *engine) checkDefinitionRefs(name string, visiting, done map[string]bool) error {
	if done[name] {
		return nil
	}
	if visiting[name] {
		return fmt.Errorf("recursive OpenAPI definition %q is unsupported for policy CEL type checking", name)
	}
	visiting[name] = true
	defer delete(visiting, name)
	root, err := e.definition(name)
	if err != nil {
		return err
	}
	if err := e.checkSchemaRefs(root, visiting, done); err != nil {
		return err
	}
	done[name] = true
	return nil
}

func (e *engine) checkSchemaRefs(node map[string]interface{}, visiting, done map[string]bool) error {
	if ref, ok := node["$ref"].(string); ok {
		const prefix = "#/definitions/"
		if !strings.HasPrefix(ref, prefix) {
			return fmt.Errorf("unsupported nonlocal OpenAPI reference %q", ref)
		}
		name := strings.ReplaceAll(strings.ReplaceAll(strings.TrimPrefix(ref, prefix), "~1", "/"), "~0", "~")
		if err := e.checkDefinitionRefs(name, visiting, done); err != nil {
			return err
		}
	}
	for _, property := range []string{"properties", "patternProperties", "definitions"} {
		if children, ok := node[property].(map[string]interface{}); ok {
			for _, value := range children {
				if child, ok := value.(map[string]interface{}); ok {
					if err := e.checkSchemaRefs(child, visiting, done); err != nil {
						return err
					}
				}
			}
		}
	}
	for _, property := range []string{"items", "additionalProperties", "additionalItems", "not"} {
		if child, ok := node[property].(map[string]interface{}); ok {
			if err := e.checkSchemaRefs(child, visiting, done); err != nil {
				return err
			}
		}
	}
	for _, property := range []string{"allOf", "anyOf", "oneOf"} {
		if children, ok := node[property].([]interface{}); ok {
			for _, value := range children {
				if child, ok := value.(map[string]interface{}); ok {
					if err := e.checkSchemaRefs(child, visiting, done); err != nil {
						return err
					}
				}
			}
		}
	}
	return nil
}

func (e *engine) policySchema(gvk schema.GroupVersionKind) (*spec.Schema, error) {
	k := key(gvk.Group, gvk.Version, gvk.Kind)
	if _, ok := e.bundle.Custom[k]; ok {
		c, err := e.customSchema(k)
		if err != nil {
			return nil, err
		}
		return c.openapi, nil
	}
	name, ok := e.bundle.Builtins[k]
	if !ok {
		return nil, fmt.Errorf("no pinned schema for group=%q kind=%q version=%q", gvk.Group, gvk.Kind, gvk.Version)
	}
	if err := e.checkDefinitionRefs(name, map[string]bool{}, map[string]bool{}); err != nil {
		return nil, err
	}
	var lookupErr error
	s, err := resolver.PopulateRefs(func(ref string) (*spec.Schema, bool) {
		const prefix = "#/definitions/"
		if strings.HasPrefix(ref, prefix) {
			ref = strings.ReplaceAll(strings.ReplaceAll(strings.TrimPrefix(ref, prefix), "~1", "/"), "~0", "~")
		}
		raw, ok := e.bundle.Definitions[ref]
		if !ok {
			lookupErr = fmt.Errorf("missing local OpenAPI definition %q", ref)
			return nil, false
		}
		var result spec.Schema
		if err := json.Unmarshal(raw, &result); err != nil {
			lookupErr = err
			return nil, false
		}
		// Swagger v2 expresses the Kubernetes int-or-string union as this
		// format; the CEL schema adapter expects the Kubernetes extension.
		if result.Format == "int-or-string" {
			result.VendorExtensible.AddExtension("x-kubernetes-int-or-string", true)
		}
		return &result, true
	}, name)
	if lookupErr != nil {
		return nil, lookupErr
	}
	if err != nil {
		return nil, err
	}
	return s, nil
}

func (e *engine) policyKinds(policy *admissionv1.ValidatingAdmissionPolicy) ([]schema.GroupVersionKind, error) {
	match := policy.Spec.MatchConstraints
	if match == nil || len(match.ResourceRules) == 0 {
		return nil, fmt.Errorf("spec.matchConstraints.resourceRules is required for offline type checking")
	}
	if len(match.ResourceRules) > maxPolicyRules {
		return nil, fmt.Errorf("too many policy resource rules (maximum %d)", maxPolicyRules)
	}
	kinds := map[string]schema.GroupVersionKind{}
	combinations := 0
	for i, rule := range match.ResourceRules {
		if len(rule.APIGroups) == 0 || len(rule.APIVersions) == 0 || len(rule.Resources) == 0 {
			return nil, fmt.Errorf("spec.matchConstraints.resourceRules[%d] has an empty group, version, or resource list", i)
		}
		for _, group := range rule.APIGroups {
			for _, version := range rule.APIVersions {
				for _, resource := range rule.Resources {
					combinations++
					if combinations > maxPolicyRuleCombinations {
						return nil, fmt.Errorf("policy resource combinations exceed %d", maxPolicyRuleCombinations)
					}
					if strings.ContainsAny(group+version+resource, "*") {
						return nil, fmt.Errorf("spec.matchConstraints.resourceRules[%d]: wildcard group/version/resource cannot be exhaustively type checked offline", i)
					}
					parts := strings.Split(resource, "/")
					if len(parts) > 2 || (len(parts) == 2 && parts[1] != "status") {
						return nil, fmt.Errorf("spec.matchConstraints.resourceRules[%d]: unsupported subresource %q", i, resource)
					}
					resourceKey := key(group, version, parts[0])
					kindKey, ok := e.bundle.Resources[resourceKey]
					if !ok {
						return nil, fmt.Errorf("no pinned type for resource group=%q version=%q resource=%q", group, version, resource)
					}
					pair := strings.SplitN(kindKey, "|", 2)
					if len(pair) != 2 {
						return nil, fmt.Errorf("invalid bundle resource mapping %q", kindKey)
					}
					gv, err := schema.ParseGroupVersion(pair[0])
					if err != nil || pair[1] == "" {
						return nil, fmt.Errorf("invalid bundle resource mapping %q", kindKey)
					}
					kinds[kindKey] = gv.WithKind(pair[1])
					if len(kinds) > maxPolicyKinds {
						return nil, fmt.Errorf("policy matches more than %d distinct kinds; offline CEL type checking is bounded", maxPolicyKinds)
					}
				}
			}
		}
	}
	if len(kinds) == 0 {
		return nil, fmt.Errorf("no policy resource kinds resolved")
	}
	keys := make([]string, 0, len(kinds))
	for k := range kinds {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	result := make([]schema.GroupVersionKind, 0, len(keys))
	for _, k := range keys {
		result = append(result, kinds[k])
	}
	return result, nil
}

func boundedPolicyExpressions(policy *admissionv1.ValidatingAdmissionPolicy) ([]labeledExpression, []labeledExpression, error) {
	var matches, expressions []labeledExpression
	totalBytes, count := 0, 0
	add := func(dst *[]labeledExpression, path, text string, returns []*celgo.Type, authorizer bool) error {
		if strings.TrimSpace(text) == "" {
			return fmt.Errorf("%s: empty CEL expression", path)
		}
		count++
		totalBytes += len(text)
		if len(text) > maxPolicyExpressionBytes || count > maxPolicyExpressions || totalBytes > maxPolicyTotalExpressionBytes {
			return fmt.Errorf("%s: offline CEL compilation limit exceeded", path)
		}
		*dst = append(*dst, labeledExpression{path: path, text: text, returns: returns, authorizer: authorizer})
		return nil
	}
	for i, match := range policy.Spec.MatchConditions {
		if err := add(&matches, fmt.Sprintf("spec.matchConditions[%d].expression", i), match.Expression, []*celgo.Type{celgo.BoolType}, true); err != nil {
			return nil, nil, err
		}
	}
	seen := map[string]bool{}
	for i, variable := range policy.Spec.Variables {
		if !celIdentifier.MatchString(variable.Name) || celReserved[variable.Name] {
			return nil, nil, fmt.Errorf("spec.variables[%d].name: invalid CEL identifier %q", i, variable.Name)
		}
		if seen[variable.Name] {
			return nil, nil, fmt.Errorf("spec.variables[%d].name: duplicate variable %q", i, variable.Name)
		}
		seen[variable.Name] = true
		if err := add(&expressions, fmt.Sprintf("spec.variables[%d].expression", i), variable.Expression, []*celgo.Type{celgo.AnyType, celgo.DynType}, true); err != nil {
			return nil, nil, err
		}
	}
	for i, validation := range policy.Spec.Validations {
		if err := add(&expressions, fmt.Sprintf("spec.validations[%d].expression", i), validation.Expression, []*celgo.Type{celgo.BoolType}, true); err != nil {
			return nil, nil, err
		}
		if validation.MessageExpression != "" {
			if err := add(&expressions, fmt.Sprintf("spec.validations[%d].messageExpression", i), validation.MessageExpression, []*celgo.Type{celgo.StringType}, false); err != nil {
				return nil, nil, err
			}
		}
	}
	for i, annotation := range policy.Spec.AuditAnnotations {
		if err := add(&expressions, fmt.Sprintf("spec.auditAnnotations[%d].valueExpression", i), annotation.ValueExpression, []*celgo.Type{celgo.StringType, celgo.NullType}, true); err != nil {
			return nil, nil, err
		}
	}
	if len(policy.Spec.Validations) == 0 && len(policy.Spec.AuditAnnotations) == 0 {
		return nil, nil, fmt.Errorf("spec.validations or spec.auditAnnotations must contain an expression")
	}
	return matches, expressions, nil
}

// Compile using the same Kubernetes admission compiler and optional variable
// declarations as a newly created policy. Variables are inserted in source
// order, so missing, forward and cyclic references fail. Match conditions are
// compiled without composed variables, as required by the API.
func compileAdmissionExpressions(policy *admissionv1.ValidatingAdmissionPolicy, matches, expressions []labeledExpression) error {
	base := environment.MustBaseEnvSet(version.MajorMinor(1, 34), true)
	matchCompiler := admissioncel.NewCompiler(base)
	var compiler admissioncel.Compiler = matchCompiler
	var composited *admissioncel.CompositedCompiler
	if len(policy.Spec.Variables) != 0 {
		var err error
		composited, err = admissioncel.NewCompositedCompiler(base)
		if err != nil {
			return err
		}
		compiler = composited
	}
	options := admissioncel.OptionalVariableDeclarations{HasParams: policy.Spec.ParamKind != nil, HasAuthorizer: true, StrictCost: true}
	for _, expr := range matches {
		result := matchCompiler.CompileCELExpression(admissionExpression{expr.text, expr.returns}, options, environment.NewExpressions)
		if result.Error != nil {
			return fmt.Errorf("%s: %v", expr.path, result.Error)
		}
	}
	for i, variable := range policy.Spec.Variables {
		result := composited.CompileAndStoreVariable(admissionVariable{variable.Name, variable.Expression}, options, environment.NewExpressions)
		if result.Error != nil {
			return fmt.Errorf("spec.variables[%d].expression: %v", i, result.Error)
		}
	}
	for _, expr := range expressions[len(policy.Spec.Variables):] {
		decls := options
		decls.HasAuthorizer = expr.authorizer
		result := compiler.CompileCELExpression(admissionExpression{expr.text, expr.returns}, decls, environment.NewExpressions)
		if result.Error != nil {
			return fmt.Errorf("%s: %v", expr.path, result.Error)
		}
	}
	return nil
}

// Use the same OpenAPI-to-CEL declarations as the upstream VAP type checker,
// but check every selected pinned kind and every expression (including the
// ordered variables and match conditions). This does not evaluate a request.
func typeCheckPolicyExpressions(policy *admissionv1.ValidatingAdmissionPolicy, gvk schema.GroupVersionKind, objectSchema, paramSchema *spec.Schema, matches, expressions []labeledExpression) error {
	objectType := common.SchemaDeclType(&openapi.Schema{Schema: objectSchema}, true)
	if objectType == nil {
		return fmt.Errorf("%v: object schema has no supported CEL type", gvk)
	}
	objectType = objectType.MaybeAssignTypeName("AdmissionObject")
	requestType, namespaceType := admissioncel.BuildRequestType(), admissioncel.BuildNamespaceType()
	declTypes := []*apicel.DeclType{objectType, requestType, namespaceType}
	opts := []celgo.EnvOption{
		celgo.Variable("object", objectType.CelType()), celgo.Variable("oldObject", objectType.CelType()),
		celgo.Variable("request", requestType.CelType()), celgo.Variable("namespaceObject", namespaceType.CelType()),
		celgo.Variable("authorizer", library.AuthorizerType), celgo.Variable("authorizer.requestResource", library.ResourceCheckType),
	}
	if paramSchema != nil {
		paramType := common.SchemaDeclType(&openapi.Schema{Schema: paramSchema}, true)
		if paramType == nil {
			return fmt.Errorf("%v: parameter schema has no supported CEL type", gvk)
		}
		paramType = paramType.MaybeAssignTypeName("AdmissionParams")
		declTypes = append(declTypes, paramType)
		opts = append(opts, celgo.Variable("params", paramType.CelType()))
	}
	base := environment.MustBaseEnvSet(version.MajorMinor(1, 34), true)
	envSet, err := base.Extend(environment.VersionedOptions{IntroducedVersion: version.MajorMinor(1, 0), EnvOptions: opts, DeclTypes: declTypes})
	if err != nil {
		return err
	}
	plain, err := envSet.Env(environment.NewExpressions)
	if err != nil {
		return err
	}
	compile := func(env *celgo.Env, expr labeledExpression) (*celgo.Ast, error) {
		ast, issues := env.Compile(expr.text)
		if issues != nil {
			return nil, fmt.Errorf("%s for %v: %s", expr.path, gvk, issues.String())
		}
		return ast, nil
	}
	for _, expr := range matches {
		if _, err := compile(plain, expr); err != nil {
			return err
		}
	}
	typed := plain
	if len(policy.Spec.Variables) != 0 {
		composition, err := admissioncel.NewCompositionEnv(admissioncel.VariablesTypeName, envSet)
		if err != nil {
			return err
		}
		typed, err = composition.Env(environment.NewExpressions)
		if err != nil {
			return err
		}
		for i, variable := range policy.Spec.Variables {
			ast, err := compile(typed, expressions[i])
			if err != nil {
				return err
			}
			composition.AddField(variable.Name, ast.OutputType())
		}
	}
	for _, expr := range expressions[len(policy.Spec.Variables):] {
		if _, err := compile(typed, expr); err != nil {
			return err
		}
	}
	return nil
}

func (e *engine) validatePolicyCEL(obj map[string]interface{}) error {
	raw, err := json.Marshal(obj)
	if err != nil {
		return err
	}
	var policy admissionv1.ValidatingAdmissionPolicy
	if err := json.Unmarshal(raw, &policy); err != nil {
		return err
	}
	matches, expressions, err := boundedPolicyExpressions(&policy)
	if err != nil {
		return err
	}
	if err := compileAdmissionExpressions(&policy, matches, expressions); err != nil {
		return err
	}
	kinds, err := e.policyKinds(&policy)
	if err != nil {
		return err
	}
	var params *spec.Schema
	if policy.Spec.ParamKind != nil {
		gv, err := schema.ParseGroupVersion(policy.Spec.ParamKind.APIVersion)
		if err != nil || policy.Spec.ParamKind.Kind == "" {
			return fmt.Errorf("invalid spec.paramKind group/version/kind")
		}
		paramGVK := gv.WithKind(policy.Spec.ParamKind.Kind)
		params, err = e.policySchema(paramGVK)
		if err != nil {
			return fmt.Errorf("spec.paramKind %v: %w", paramGVK, err)
		}
	}
	for _, gvk := range kinds {
		objectSchema, err := e.policySchema(gvk)
		if err != nil {
			return fmt.Errorf("policy target %v: %w", gvk, err)
		}
		if err := typeCheckPolicyExpressions(&policy, gvk, objectSchema, params, matches, expressions); err != nil {
			return err
		}
	}
	return nil
}
