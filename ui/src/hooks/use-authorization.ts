import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  listPolicies,
  createPolicy,
  deletePolicy,
  assignRole,
  getRolesByUserDomain,
  removeRole,
} from "@/services/authorization";
import { CreatePolicyRequest, CreateRoleRequest } from "@/lib/types";

// ─── Policies ──────────────────────────────────────────────

export function usePolicies() {
  return useQuery({
    queryKey: ["policies"],
    queryFn: listPolicies,
    select: (data) => data.policies,
  });
}

export function useCreatePolicy() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (data: CreatePolicyRequest) => createPolicy(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["policies"] });
    },
  });
}

export function useDeletePolicy() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      subject,
      domain,
      object,
      action,
    }: {
      subject: string;
      domain: string;
      object: string;
      action: string;
    }) => deletePolicy(subject, domain, object, action),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["policies"] });
    },
  });
}

// ─── Roles ─────────────────────────────────────────────────

export function useRolesByUserDomain(user: string, domain: string) {
  return useQuery({
    queryKey: ["roles", user, domain],
    queryFn: () => getRolesByUserDomain(user, domain),
    enabled: !!user && !!domain,
  });
}

export function useAssignRole() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (data: CreateRoleRequest) => assignRole(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["roles"] });
    },
  });
}

export function useRemoveRole() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      user,
      domain,
      role,
    }: {
      user: string;
      domain: string;
      role: string;
    }) => removeRole(user, domain, role),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["roles"] });
    },
  });
}
