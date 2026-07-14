import { apiClient } from "@/lib/api-client";
import {
	type CreatePolicyRequest,
	type CreateRoleRequest,
	type PoliciesListResponse,
	type PolicyInfo,
	RemoveRoleRequest,
	type RoleResponse,
	type RolesListResponse,
} from "@/lib/types";

// ─── Policies ──────────────────────────────────────────────

export async function listPolicies(): Promise<PoliciesListResponse> {
	return apiClient.get<PoliciesListResponse>("/admin/authz/policies");
}

export async function getPoliciesBySubjectDomain(
	subject: string,
	domain: string,
): Promise<PoliciesListResponse> {
	return apiClient.get<PoliciesListResponse>(
		`/admin/authz/policies/${encodeURIComponent(subject)}/${encodeURIComponent(domain)}`,
	);
}

export async function createPolicy(
	data: CreatePolicyRequest,
): Promise<PolicyInfo> {
	return apiClient.post<PolicyInfo>("/admin/authz/policies", data);
}

export async function deletePolicy(
	subject: string,
	domain: string,
	object: string,
	action: string,
): Promise<void> {
	await apiClient.delete(
		`/admin/authz/policies/${encodeURIComponent(subject)}/${encodeURIComponent(domain)}/${encodeURIComponent(object)}/${encodeURIComponent(action)}`,
	);
}

// ─── Roles ─────────────────────────────────────────────────

export async function assignRole(
	data: CreateRoleRequest,
): Promise<RoleResponse> {
	return apiClient.post<RoleResponse>("/admin/authz/roles", data);
}

export async function getRolesByUserDomain(
	user: string,
	domain: string,
): Promise<RolesListResponse> {
	return apiClient.get<RolesListResponse>(
		`/admin/authz/roles/${encodeURIComponent(user)}/${encodeURIComponent(domain)}`,
	);
}

export async function removeRole(
	user: string,
	domain: string,
	role: string,
): Promise<void> {
	await apiClient.delete(
		`/admin/authz/roles/${encodeURIComponent(user)}/${encodeURIComponent(domain)}`,
		{
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ role }),
		},
	);
}
