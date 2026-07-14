import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { CreateApiKeyRequest } from "@/lib/types";
import {
	createApiKey,
	listApiKeys,
	revokeApiKey,
	rotateApiKey,
} from "@/services/api-keys";

export function useApiKeys() {
	return useQuery({
		queryKey: ["api-keys"],
		queryFn: listApiKeys,
	});
}

export function useCreateApiKey() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (data: CreateApiKeyRequest) => createApiKey(data),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["api-keys"] });
		},
	});
}

export function useRevokeApiKey() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (keyId: string) => revokeApiKey(keyId),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["api-keys"] });
		},
	});
}

export function useRotateApiKey() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: ({
			keyId,
			gracePeriodMinutes,
			reason,
		}: {
			keyId: string;
			gracePeriodMinutes?: number;
			reason?: string;
		}) => rotateApiKey(keyId, gracePeriodMinutes, reason),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["api-keys"] });
		},
	});
}
