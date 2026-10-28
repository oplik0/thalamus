package main

import (
	"strings"
	"testing"

	admissionv1 "k8s.io/api/admissionregistration/v1"
)

// These exercise compilation only; no API server or network is involved.
func TestAdmissionPolicyCELCompilation(t *testing.T) {
	tests := []struct {
		name        string
		variables   []admissionv1.Variable
		validations []admissionv1.Validation
		matches     []admissionv1.MatchCondition
		audit       []admissionv1.AuditAnnotation
		wantError   string
	}{
		{
			name:        "ordered variables and all expression kinds",
			variables:   []admissionv1.Variable{{Name: "first", Expression: "1"}, {Name: "second", Expression: "variables.first + 1"}},
			validations: []admissionv1.Validation{{Expression: "variables.second == 2", MessageExpression: `"not valid"`}},
			matches:     []admissionv1.MatchCondition{{Name: "create", Expression: `request.operation == "CREATE"`}},
			audit:       []admissionv1.AuditAnnotation{{Key: "result", ValueExpression: `"checked"`}},
		},
		{
			name:        "forward variable reference",
			variables:   []admissionv1.Variable{{Name: "first", Expression: "variables.second"}, {Name: "second", Expression: "1"}},
			validations: []admissionv1.Validation{{Expression: "true"}},
			wantError:   "spec.variables[0].expression",
		},
		{
			name:        "unknown variable",
			variables:   []admissionv1.Variable{{Name: "first", Expression: "1"}},
			validations: []admissionv1.Validation{{Expression: "variables.unknown == 1"}},
			wantError:   "spec.validations[0].expression",
		},
		{
			name:        "match conditions cannot use variables",
			variables:   []admissionv1.Variable{{Name: "first", Expression: "true"}},
			matches:     []admissionv1.MatchCondition{{Name: "match", Expression: "variables.first"}},
			validations: []admissionv1.Validation{{Expression: "true"}},
			wantError:   "spec.matchConditions[0].expression",
		},
		{
			name:        "validation must return boolean",
			validations: []admissionv1.Validation{{Expression: "1"}},
			wantError:   "spec.validations[0].expression",
		},
		{
			name:        "message must return string",
			validations: []admissionv1.Validation{{Expression: "true", MessageExpression: "1"}},
			wantError:   "spec.validations[0].messageExpression",
		},
		{
			name:        "audit expression must return string or null",
			validations: []admissionv1.Validation{{Expression: "true"}},
			audit:       []admissionv1.AuditAnnotation{{Key: "result", ValueExpression: "true"}},
			wantError:   "spec.auditAnnotations[0].valueExpression",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			policy := &admissionv1.ValidatingAdmissionPolicy{Spec: admissionv1.ValidatingAdmissionPolicySpec{
				Variables: test.variables, Validations: test.validations, MatchConditions: test.matches, AuditAnnotations: test.audit,
			}}
			matches, expressions, err := boundedPolicyExpressions(policy)
			if err == nil {
				err = compileAdmissionExpressions(policy, matches, expressions)
			}
			if test.wantError == "" && err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if test.wantError != "" && (err == nil || !strings.Contains(err.Error(), test.wantError)) {
				t.Fatalf("expected error at %s, got %v", test.wantError, err)
			}
		})
	}
}
