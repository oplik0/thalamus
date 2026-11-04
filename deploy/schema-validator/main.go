// Offline Kubernetes schema/create-CEL validator. Preparation and usage are in
// ../prepare-schema-bundle.py. It never loads schemas from URLs or a cluster.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"

	apiextensions "k8s.io/apiextensions-apiserver/pkg/apis/apiextensions"
	apiextensionsv1 "k8s.io/apiextensions-apiserver/pkg/apis/apiextensions/v1"
	structural "k8s.io/apiextensions-apiserver/pkg/apiserver/schema"
	structuralcel "k8s.io/apiextensions-apiserver/pkg/apiserver/schema/cel"
	"k8s.io/apiextensions-apiserver/pkg/apiserver/schema/defaulting"
	"k8s.io/apiextensions-apiserver/pkg/apiserver/schema/listtype"
	"k8s.io/apiextensions-apiserver/pkg/apiserver/schema/objectmeta"
	"k8s.io/apiextensions-apiserver/pkg/apiserver/schema/pruning"
	crvalidation "k8s.io/apiextensions-apiserver/pkg/apiserver/validation"
	"k8s.io/apimachinery/pkg/util/validation/field"
	kyaml "k8s.io/apimachinery/pkg/util/yaml"
	celconfig "k8s.io/apiserver/pkg/apis/cel"
	"k8s.io/kube-openapi/pkg/validation/spec"
	"k8s.io/kube-openapi/pkg/validation/strfmt"
	"k8s.io/kube-openapi/pkg/validation/validate"
)

type bundle struct {
	Format      int                        `json:"format"`
	Kubernetes  string                     `json:"kubernetes"`
	Definitions map[string]json.RawMessage `json:"definitions"`
	Builtins    map[string]string          `json:"builtins"`
	Resources   map[string]string          `json:"resources"`
	Custom      map[string]json.RawMessage `json:"custom"`
}

type compiledCRD struct {
	structure *structural.Structural
	openapi   *spec.Schema
	validator crvalidation.SchemaValidator
	cel       *structuralcel.Validator
}

type engine struct {
	bundle      bundle
	cache       map[string]*compiledCRD
	definitions map[string]map[string]interface{}
	count       int
}

func key(group, version, kind string) string {
	if group == "" {
		return version + "|" + kind
	}
	return group + "/" + version + "|" + kind
}

// Read one YAML/JSON document at a time, preserving integers and rejecting
// duplicate mapping keys. Blank documents are not resources.
func documents(r io.Reader, visit func(interface{}, int) error) error {
	reader := kyaml.NewYAMLReader(bufio.NewReader(r))
	for n := 1; ; n++ {
		data, err := reader.Read()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return fmt.Errorf("document %d: %w", n, err)
		}
		var obj interface{}
		if err := kyaml.UnmarshalStrict(data, &obj); err != nil {
			return fmt.Errorf("document %d: %w", n, err)
		}
		if obj != nil {
			if err := visit(obj, n); err != nil {
				return err
			}
		}
	}
}

// Preparing a bundle extracts exactly the served-version schemas from the
// verified upstream CRDs. The upstream manifests are inputs, not loose schemas
// invented by this tool. Unserved versions never enter the index.
func prepare(dir, output string) error {
	data, err := os.ReadFile(filepath.Join(dir, "kubernetes-openapi.json"))
	if err != nil {
		return err
	}
	var swagger struct {
		Definitions map[string]json.RawMessage            `json:"definitions"`
		Paths       map[string]map[string]json.RawMessage `json:"paths"`
	}
	if err := json.Unmarshal(data, &swagger); err != nil {
		return err
	}
	b := bundle{Format: 2, Kubernetes: "v1.34.2", Definitions: swagger.Definitions,
		Builtins: map[string]string{}, Custom: map[string]json.RawMessage{}, Resources: map[string]string{}}
	for name, raw := range b.Definitions {
		var info struct {
			GVK []struct{ Group, Version, Kind string } `json:"x-kubernetes-group-version-kind"`
		}
		if err := json.Unmarshal(raw, &info); err != nil {
			return err
		}
		for _, gvk := range info.GVK {
			k := key(gvk.Group, gvk.Version, gvk.Kind)
			if existing, ok := b.Builtins[k]; ok && existing != name {
				return fmt.Errorf("duplicate builtin schema %s: %s and %s", k, existing, name)
			}
			b.Builtins[k] = name
		}
	}
	// The official OpenAPI collection paths carry the actual plural resource
	// name and GVK. Never guess plural forms from Kind names.
	for path, operations := range swagger.Paths {
		for _, method := range []string{"get", "post"} {
			raw, ok := operations[method]
			if !ok {
				continue
			}
			var operation struct {
				Action string                                `json:"x-kubernetes-action"`
				GVK    struct{ Group, Version, Kind string } `json:"x-kubernetes-group-version-kind"`
			}
			if err := json.Unmarshal(raw, &operation); err != nil {
				return err
			}
			if (method != "get" || operation.Action != "list") && (method != "post" || operation.Action != "post") {
				continue
			}
			gvk := operation.GVK
			if gvk.Version == "" || gvk.Kind == "" {
				continue
			}
			prefix := "/api/" + gvk.Version + "/"
			if gvk.Group != "" {
				prefix = "/apis/" + gvk.Group + "/" + gvk.Version + "/"
			}
			if !strings.HasPrefix(path, prefix) {
				continue
			}
			parts := strings.Split(strings.TrimPrefix(path, prefix), "/")
			var resource string
			if len(parts) == 1 {
				resource = parts[0]
			} else if len(parts) == 3 && parts[0] == "namespaces" && parts[1] == "{namespace}" {
				resource = parts[2]
			} else {
				continue
			}
			if strings.ContainsAny(resource, "{}") {
				continue
			}
			resourceKey, kindKey := key(gvk.Group, gvk.Version, resource), key(gvk.Group, gvk.Version, gvk.Kind)
			if existing, ok := b.Resources[resourceKey]; ok && existing != kindKey {
				return fmt.Errorf("ambiguous resource mapping %s: %s and %s", resourceKey, existing, kindKey)
			}
			b.Resources[resourceKey] = kindKey
		}
	}
	files, err := filepath.Glob(filepath.Join(dir, "*.yaml"))
	if err != nil {
		return err
	}
	for _, file := range files {
		f, err := os.Open(file)
		if err != nil {
			return err
		}
		err = documents(f, func(obj interface{}, n int) error {
			m, ok := obj.(map[string]interface{})
			if !ok || m["apiVersion"] != "apiextensions.k8s.io/v1" || m["kind"] != "CustomResourceDefinition" {
				return nil
			}
			raw, err := json.Marshal(m)
			if err != nil {
				return err
			}
			var crd apiextensionsv1.CustomResourceDefinition
			if err := json.Unmarshal(raw, &crd); err != nil {
				return fmt.Errorf("%s document %d: %w", file, n, err)
			}
			if crd.Spec.Group == "" || crd.Spec.Names.Kind == "" {
				return fmt.Errorf("%s document %d: CRD has no group/kind", file, n)
			}
			for _, v := range crd.Spec.Versions {
				if !v.Served {
					continue
				}
				k := key(crd.Spec.Group, v.Name, crd.Spec.Names.Kind)
				if crd.Spec.Names.Plural == "" {
					return fmt.Errorf("%s: CRD has no plural resource name", k)
				}
				resourceKey := key(crd.Spec.Group, v.Name, crd.Spec.Names.Plural)
				if existing, ok := b.Resources[resourceKey]; ok && existing != k {
					return fmt.Errorf("ambiguous resource mapping %s: %s and %s", resourceKey, existing, k)
				}
				b.Resources[resourceKey] = k
				if v.Schema == nil || v.Schema.OpenAPIV3Schema == nil {
					return fmt.Errorf("%s: served CRD has no schema", k)
				}
				if _, ok := b.Custom[k]; ok {
					return fmt.Errorf("duplicate CRD schema %s", k)
				}
				// Preserve the original schema exactly, including vendor extensions.
				specMap, _ := m["spec"].(map[string]interface{})
				versions, _ := specMap["versions"].([]interface{})
				for _, item := range versions {
					vm, ok := item.(map[string]interface{})
					if !ok || vm["name"] != v.Name {
						continue
					}
					sm, _ := vm["schema"].(map[string]interface{})
					schema := sm["openAPIV3Schema"]
					encoded, err := json.Marshal(schema)
					if err != nil {
						return err
					}
					b.Custom[k] = encoded
				}
				if b.Custom[k] == nil {
					return fmt.Errorf("%s: schema extraction failed", k)
				}
			}
			return nil
		})
		f.Close()
		if err != nil {
			return err
		}
	}
	for _, required := range []string{
		"gateway.networking.k8s.io/v1|Gateway", "gateway.networking.k8s.io/v1|HTTPRoute",
		"inference.networking.k8s.io/v1|InferencePool", "serving.kserve.io/v1alpha2|LLMInferenceService",
		"agentgateway.dev/v1alpha1|AgentgatewayPolicy", "agentgateway.dev/v1alpha1|AgentgatewayBackend",
		"source.toolkit.fluxcd.io/v1|GitRepository", "source.toolkit.fluxcd.io/v1|OCIRepository",
		"kustomize.toolkit.fluxcd.io/v1|Kustomization", "helm.toolkit.fluxcd.io/v2|HelmRelease",
		"kueue.x-k8s.io/v1beta2|ClusterQueue", "kueue.x-k8s.io/v1beta2|LocalQueue",
		"kueue.x-k8s.io/v1beta2|ResourceFlavor", "keda.sh/v1alpha1|ScaledObject", "llm-d.ai/v1|InferenceObjective",
	} {
		if _, ok := b.Custom[required]; !ok {
			return fmt.Errorf("required pinned CRD is absent or unserved: %s", required)
		}
	}
	for _, required := range []string{"v1|Namespace", "apps/v1|Deployment", "admissionregistration.k8s.io/v1|ValidatingAdmissionPolicy"} {
		if _, ok := b.Builtins[required]; !ok {
			return fmt.Errorf("required Kubernetes schema missing: %s", required)
		}
	}
	encoded, err := json.Marshal(b)
	if err != nil {
		return err
	}
	if err := os.WriteFile(output, encoded, 0o644); err != nil {
		return err
	}
	fmt.Printf("prepared Kubernetes %s: %d builtin and %d served CRD schemas\n", b.Kubernetes, len(b.Builtins), len(b.Custom))
	return nil
}

var crdSchemaKeywords = map[string]bool{
	"$schema": true, "id": true, "$ref": true, "description": true, "type": true,
	"format": true, "title": true, "default": true, "nullable": true,
	"maximum": true, "exclusiveMaximum": true, "minimum": true, "exclusiveMinimum": true,
	"maxLength": true, "minLength": true, "pattern": true, "maxItems": true,
	"minItems": true, "uniqueItems": true, "multipleOf": true, "maxProperties": true,
	"minProperties": true, "required": true, "items": true, "allOf": true,
	"oneOf": true, "anyOf": true, "not": true, "properties": true,
	"additionalProperties": true, "patternProperties": true, "dependencies": true,
	"additionalItems": true, "definitions": true, "externalDocs": true, "example": true,
	"enum": true, "x-kubernetes-preserve-unknown-fields": true, "x-kubernetes-embedded-resource": true,
	"x-kubernetes-int-or-string": true, "x-kubernetes-list-type": true,
	"x-kubernetes-list-map-keys": true, "x-kubernetes-map-type": true, "x-kubernetes-validations": true,
}

// Detect unrecognized keywords before typed decoding can discard them. The
// apiserver structural conversion also rejects unsupported CRD features.
func checkCRDSchema(node interface{}, path string) error {
	m, ok := node.(map[string]interface{})
	if !ok {
		return fmt.Errorf("%s: schema must be an object", path)
	}
	for k, v := range m {
		if !crdSchemaKeywords[k] {
			return fmt.Errorf("%s: unsupported schema keyword %s", path, k)
		}
		switch k {
		case "properties", "patternProperties", "definitions":
			if sub, ok := v.(map[string]interface{}); ok {
				for name, schema := range sub {
					if err := checkCRDSchema(schema, path+"."+k+"."+name); err != nil {
						return err
					}
				}
			}
		case "items", "not", "additionalProperties", "additionalItems":
			if _, ok := v.(map[string]interface{}); ok {
				if err := checkCRDSchema(v, path+"."+k); err != nil {
					return err
				}
			}
		case "allOf", "anyOf", "oneOf":
			if a, ok := v.([]interface{}); ok {
				for i, s := range a {
					if err := checkCRDSchema(s, fmt.Sprintf("%s.%s[%d]", path, k, i)); err != nil {
						return err
					}
				}
			}
		}
	}
	return nil
}

func (e *engine) customSchema(k string) (*compiledCRD, error) {
	if c := e.cache[k]; c != nil {
		return c, nil
	}
	raw := e.bundle.Custom[k]
	var generic interface{}
	if err := json.Unmarshal(raw, &generic); err != nil {
		return nil, err
	}
	if err := checkCRDSchema(generic, k); err != nil {
		return nil, err
	}
	var external apiextensionsv1.JSONSchemaProps
	if err := json.Unmarshal(raw, &external); err != nil {
		return nil, err
	}
	var internal apiextensions.JSONSchemaProps
	if err := apiextensionsv1.Convert_v1_JSONSchemaProps_To_apiextensions_JSONSchemaProps(&external, &internal, nil); err != nil {
		return nil, err
	}
	s, err := structural.NewStructural(&internal)
	if err != nil || s == nil {
		return nil, fmt.Errorf("%s: unsupported structural schema: %v", k, err)
	}
	validator, openapi, err := crvalidation.NewSchemaValidator(&internal)
	if err != nil {
		return nil, err
	}
	c := &compiledCRD{structure: s, openapi: openapi, validator: validator, cel: structuralcel.NewValidator(s, true, celconfig.PerCallLimit)}
	e.cache[k] = c
	return c, nil
}

func validateCustom(c *compiledCRD, obj map[string]interface{}) error {
	path := field.NewPath("resource")
	metaErr, metaUnknown := objectmeta.CoerceWithOptions(path, obj, c.structure, true, objectmeta.CoerceOptions{ReturnUnknownFieldPaths: true})
	if metaErr != nil {
		return metaErr
	}
	if len(metaUnknown) != 0 {
		return fmt.Errorf("unknown metadata fields: %s", strings.Join(metaUnknown, ", "))
	}
	unknown := pruning.PruneWithOptions(obj, c.structure, true, structural.UnknownFieldPathOptions{TrackUnknownFieldPaths: true})
	if len(unknown) != 0 {
		return fmt.Errorf("unknown/pruned fields: %s", strings.Join(unknown, ", "))
	}
	defaulting.PruneNonNullableNullsWithoutDefaults(obj, c.structure)
	defaulting.Default(obj, c.structure)
	if errs := crvalidation.ValidateCustomResource(path, obj, c.validator); len(errs) != 0 {
		return errs.ToAggregate()
	}
	if errs := listtype.ValidateListSetsAndMaps(path, c.structure, obj); len(errs) != 0 {
		return errs.ToAggregate()
	}
	if c.cel != nil {
		errs, _ := c.cel.Validate(context.Background(), path, c.structure, obj, nil, celconfig.RuntimeCELCostBudget)
		if len(errs) != 0 {
			return errs.ToAggregate()
		}
	}
	return nil
}

func samplesForProperty(values []interface{}, name string) []interface{} {
	var out []interface{}
	for _, v := range values {
		if m, ok := v.(map[string]interface{}); ok {
			if x, found := m[name]; found {
				out = append(out, x)
			}
		}
	}
	return out
}

func (e *engine) definition(name string) (map[string]interface{}, error) {
	if m := e.definitions[name]; m != nil {
		return m, nil
	}
	raw, ok := e.bundle.Definitions[name]
	if !ok {
		return nil, fmt.Errorf("missing local definition %s", name)
	}
	var m map[string]interface{}
	if err := json.Unmarshal(raw, &m); err != nil {
		return nil, err
	}
	if e.definitions == nil {
		e.definitions = make(map[string]map[string]interface{})
	}
	e.definitions[name] = m
	return m, nil
}

// Resolve only local references. Expand the part of a recursive schema reached
// by this input, so cyclic OpenAPI definitions cannot trigger network access
// or infinite recursion. Validation rules at every visited node are retained.
func (e *engine) expand(node map[string]interface{}, values []interface{}, path string, depth int) (map[string]interface{}, error) {
	if len(values) == 0 {
		return map[string]interface{}{}, nil
	}
	if depth > 256 {
		return nil, fmt.Errorf("%s: schema/reference nesting exceeds 256", path)
	}
	if ref, ok := node["$ref"].(string); ok {
		const prefix = "#/definitions/"
		if !strings.HasPrefix(ref, prefix) {
			return nil, fmt.Errorf("%s: unsupported nonlocal reference %q", path, ref)
		}
		for k := range node {
			switch k {
			case "$ref", "description", "title", "x-kubernetes-patch-strategy", "x-kubernetes-map-type", "x-kubernetes-list-type":
				// These are documentation or patch/merge annotations, not value constraints.
			default:
				return nil, fmt.Errorf("%s: unsupported $ref sibling %s", path, k)
			}
		}
		name := strings.ReplaceAll(strings.ReplaceAll(strings.TrimPrefix(ref, prefix), "~1", "/"), "~0", "~")
		target, err := e.definition(name)
		if err != nil {
			return nil, err
		}
		return e.expand(target, values, path, depth+1)
	}
	out := make(map[string]interface{}, len(node)+1)
	for k, v := range node {
		out[k] = v
	}
	for k := range node {
		if !strings.HasPrefix(k, "x-") {
			continue
		}
		switch k {
		case "x-kubernetes-group-version-kind", "x-kubernetes-list-type", "x-kubernetes-list-map-keys", "x-kubernetes-map-type", "x-kubernetes-patch-strategy", "x-kubernetes-patch-merge-key", "x-kubernetes-int-or-string", "x-kubernetes-preserve-unknown-fields":
		case "x-kubernetes-unions":
			if err := checkUnions(node[k], values, path); err != nil {
				return nil, err
			}
		default:
			return nil, fmt.Errorf("%s: unsupported builtin schema extension %s", path, k)
		}
	}
	if node["x-kubernetes-int-or-string"] == true || node["format"] == "int-or-string" {
		out["type"] = []string{"integer", "string"}
		delete(out, "format")
	}
	if typ, _ := node["x-kubernetes-list-type"].(string); typ != "" {
		if err := checkList(typ, node["x-kubernetes-list-map-keys"], values, path); err != nil {
			return nil, err
		}
	}
	props, hasProps := node["properties"].(map[string]interface{})
	if hasProps {
		processed := map[string]interface{}{}
		for name, child := range props {
			cm, ok := child.(map[string]interface{})
			if !ok {
				return nil, fmt.Errorf("%s.%s: malformed schema", path, name)
			}
			x, err := e.expand(cm, samplesForProperty(values, name), path+"."+name, depth+1)
			if err != nil {
				return nil, err
			}
			processed[name] = x
		}
		out["properties"] = processed
		if _, explicit := node["additionalProperties"]; !explicit && node["x-kubernetes-preserve-unknown-fields"] != true {
			out["additionalProperties"] = false
		}
	}
	if child, ok := node["additionalProperties"].(map[string]interface{}); ok {
		var unknown []interface{}
		for _, v := range values {
			if m, ok := v.(map[string]interface{}); ok {
				for name, x := range m {
					if _, known := props[name]; !known {
						unknown = append(unknown, x)
					}
				}
			}
		}
		x, err := e.expand(child, unknown, path+".*", depth+1)
		if err != nil {
			return nil, err
		}
		out["additionalProperties"] = x
	}
	if child, ok := node["items"].(map[string]interface{}); ok {
		var items []interface{}
		for _, v := range values {
			if a, ok := v.([]interface{}); ok {
				items = append(items, a...)
			}
		}
		x, err := e.expand(child, items, path+"[]", depth+1)
		if err != nil {
			return nil, err
		}
		out["items"] = x
	}
	for _, fieldName := range []string{"allOf", "anyOf", "oneOf"} {
		if parts, ok := node[fieldName].([]interface{}); ok {
			result := make([]interface{}, 0, len(parts))
			for _, part := range parts {
				child, ok := part.(map[string]interface{})
				if !ok {
					return nil, fmt.Errorf("%s: malformed %s", path, fieldName)
				}
				x, err := e.expand(child, values, path+"."+fieldName, depth+1)
				if err != nil {
					return nil, err
				}
				result = append(result, x)
			}
			out[fieldName] = result
		}
	}
	if child, ok := node["not"].(map[string]interface{}); ok {
		x, err := e.expand(child, values, path+".not", depth+1)
		if err != nil {
			return nil, err
		}
		out["not"] = x
	}
	return out, nil
}

// Kubernetes builtin unions contain discriminator hints but do not specify
// complete admission rules. Check mutual exclusion; fail closed if a selected
// member needs discriminator interpretation, rather than claim it was checked.
func checkUnions(raw interface{}, values []interface{}, path string) error {
	unions, ok := raw.([]interface{})
	if !ok {
		return fmt.Errorf("%s: malformed x-kubernetes-unions", path)
	}
	for _, u := range unions {
		union, ok := u.(map[string]interface{})
		if !ok {
			return fmt.Errorf("%s: malformed union", path)
		}
		members, ok := union["fields-to-discriminateBy"].(map[string]interface{})
		if !ok {
			return fmt.Errorf("%s: unsupported union shape", path)
		}
		for _, v := range values {
			obj, ok := v.(map[string]interface{})
			if !ok {
				continue
			}
			count := 0
			for member := range members {
				if _, found := obj[member]; found {
					count++
				}
			}
			if count > 1 {
				return fmt.Errorf("%s: mutually exclusive union members supplied", path)
			}
			if count == 1 {
				return fmt.Errorf("%s: unsupported builtin union discriminator validation", path)
			}
		}
	}
	return nil
}

func checkList(typ string, rawKeys interface{}, values []interface{}, path string) error {
	if typ == "atomic" {
		return nil
	}
	if typ != "set" && typ != "map" {
		return fmt.Errorf("%s: unsupported list type %q", path, typ)
	}
	var keys []string
	if typ == "map" {
		list, ok := rawKeys.([]interface{})
		if !ok || len(list) == 0 {
			return fmt.Errorf("%s: malformed list-map-keys", path)
		}
		for _, k := range list {
			s, ok := k.(string)
			if !ok {
				return fmt.Errorf("%s: malformed list-map key", path)
			}
			keys = append(keys, s)
		}
	}
	for _, value := range values {
		items, ok := value.([]interface{})
		if !ok || len(items) < 2 {
			continue
		}
		if typ == "set" {
			seen := map[string]bool{}
			for _, item := range items {
				encoded, err := json.Marshal(item)
				if err != nil {
					return err
				}
				id := string(encoded)
				if seen[id] {
					return fmt.Errorf("%s: duplicate set list item", path)
				}
				seen[id] = true
			}
			continue
		}
		// Some built-in list keys (for example protocol) are defaulted by the
		// API server but have no OpenAPI default. Distinct known keys prove
		// uniqueness without inventing a default. Ambiguous cases fail closed.
		for i := range items {
			left, ok := items[i].(map[string]interface{})
			if !ok {
				continue
			}
			for j := range i {
				right, ok := items[j].(map[string]interface{})
				if !ok {
					continue
				}
				different, incomplete := false, false
				for _, k := range keys {
					l, lok := left[k]
					r, rok := right[k]
					if !lok || !rok {
						incomplete = true
						continue
					}
					a, err := json.Marshal(l)
					if err != nil {
						return err
					}
					b, err := json.Marshal(r)
					if err != nil {
						return err
					}
					if string(a) != string(b) {
						different = true
						break
					}
				}
				if different {
					continue
				}
				if incomplete {
					return fmt.Errorf("%s: cannot determine list-map uniqueness before API-server defaults", path)
				}
				return fmt.Errorf("%s: duplicate map list item", path)
			}
		}
	}
	return nil
}

func (e *engine) validateBuiltin(name string, obj map[string]interface{}) error {
	schema, err := e.definition(name)
	if err != nil {
		return err
	}
	expanded, err := e.expand(schema, []interface{}{obj}, name, 0)
	if err != nil {
		return err
	}
	raw, err := json.Marshal(expanded)
	if err != nil {
		return err
	}
	var openapi spec.Schema
	if err := json.Unmarshal(raw, &openapi); err != nil {
		return err
	}
	result := validate.NewSchemaValidator(&openapi, nil, "", strfmt.Default).Validate(obj)
	if !result.IsValid() {
		return fmt.Errorf("%v", result.Errors)
	}
	return nil
}

func (e *engine) resource(value interface{}, where string) error {
	obj, ok := value.(map[string]interface{})
	if !ok {
		return fmt.Errorf("%s: expected Kubernetes resource object", where)
	}
	api, apiOK := obj["apiVersion"].(string)
	kind, kindOK := obj["kind"].(string)
	if !apiOK || api == "" || !kindOK || kind == "" {
		return fmt.Errorf("%s: resource requires nonempty apiVersion and kind (do not pass catalog or lock data)", where)
	}
	if api == "v1" && kind == "List" {
		for fieldName := range obj {
			switch fieldName {
			case "apiVersion", "kind", "metadata", "items":
			default:
				return fmt.Errorf("%s: unknown v1/List field %q", where, fieldName)
			}
		}
		if metadata, found := obj["metadata"]; found {
			m, ok := metadata.(map[string]interface{})
			if !ok {
				return fmt.Errorf("%s: v1/List metadata must be an object", where)
			}
			if err := e.validateBuiltin("io.k8s.apimachinery.pkg.apis.meta.v1.ListMeta", m); err != nil {
				return fmt.Errorf("%s: invalid v1/List metadata: %w", where, err)
			}
		}
		items, ok := obj["items"].([]interface{})
		if !ok {
			return fmt.Errorf("%s: v1/List items must be an array", where)
		}
		for i, item := range items {
			if err := e.resource(item, fmt.Sprintf("%s.items[%d]", where, i)); err != nil {
				return err
			}
		}
		return nil
	}
	parts := strings.Split(api, "/")
	group, version := "", ""
	if len(parts) == 1 {
		version = parts[0]
	} else if len(parts) == 2 {
		group, version = parts[0], parts[1]
	}
	if version == "" || (len(parts) == 2 && group == "") || len(parts) > 2 {
		return fmt.Errorf("%s: malformed apiVersion %q for kind %q", where, api, kind)
	}
	k := key(group, version, kind)
	var err error
	if _, ok := e.bundle.Custom[k]; ok {
		var c *compiledCRD
		c, err = e.customSchema(k)
		if err == nil {
			err = validateCustom(c, obj)
		}
	} else if name, ok := e.bundle.Builtins[k]; ok {
		err = e.validateBuiltin(name, obj)
	} else {
		return fmt.Errorf("%s: no pinned schema for group=%q kind=%q version=%q", where, group, kind, version)
	}
	if err == nil && api == "admissionregistration.k8s.io/v1" && kind == "ValidatingAdmissionPolicy" {
		err = e.validatePolicyCEL(obj)
	}
	if err != nil {
		return fmt.Errorf("%s: group=%q kind=%q version=%q: %w", where, group, kind, version, err)
	}
	e.count++
	return nil
}

func run() error {
	bundleFile := flag.String("bundle", "", "prepared local schemas.json")
	prepareDir := flag.String("prepare", "", "internal: extract schemas from verified sources")
	flag.Parse()
	if *bundleFile == "" {
		return errors.New("--bundle is required")
	}
	if *prepareDir != "" {
		if flag.NArg() != 0 {
			return errors.New("--prepare does not take manifest arguments")
		}
		return prepare(*prepareDir, *bundleFile)
	}
	if flag.NArg() == 0 {
		return errors.New("provide at least one rendered Kubernetes YAML/JSON file")
	}
	data, err := os.ReadFile(*bundleFile)
	if err != nil {
		return err
	}
	var b bundle
	if err := json.Unmarshal(data, &b); err != nil {
		return err
	}
	if b.Format != 2 || b.Kubernetes != "v1.34.2" || len(b.Builtins) == 0 || len(b.Custom) == 0 || len(b.Resources) == 0 {
		return errors.New("invalid or incompatible schema bundle; prepare it again")
	}
	e := engine{bundle: b, cache: map[string]*compiledCRD{}}
	var failures []string
	for _, file := range flag.Args() {
		f, err := os.Open(file)
		if err != nil {
			failures = append(failures, err.Error())
			continue
		}
		err = documents(f, func(obj interface{}, n int) error { return e.resource(obj, fmt.Sprintf("%s:document %d", file, n)) })
		f.Close()
		if err != nil {
			failures = append(failures, err.Error())
		}
	}
	if len(failures) != 0 {
		sort.Strings(failures)
		return errors.New(strings.Join(failures, "\n"))
	}
	if e.count == 0 {
		return errors.New("no Kubernetes resources found")
	}
	fmt.Printf("validated %d resources using Kubernetes %s and pinned CRDs (offline schema and bounded CEL compilation only)\n", e.count, b.Kubernetes)
	return nil
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "schema validation:", err)
		os.Exit(1)
	}
}
