import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { CreateSigningKeyRequest } from "@/lib/types";
import {
	createSigningKey,
	listSigningKeys,
	revokeSigningKey,
} from "@/services/signing-keys";

export function useSigningKeys() {
	return useQuery({
		queryKey: ["signing-keys"],
		queryFn: listSigningKeys,
		select: (data) => data.keys,
	});
}

export function useCreateSigningKey() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (data: CreateSigningKeyRequest) => createSigningKey(data),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["signing-keys"] });
		},
	});
}

export function useRevokeSigningKey() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (keyId: string) => revokeSigningKey(keyId),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["signing-keys"] });
		},
	});
}
