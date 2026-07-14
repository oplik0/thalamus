import { apiClient } from "@/lib/api-client";
import type {
	CreateSigningKeyRequest,
	CreateSigningKeyResponse,
	SigningKeyDetailInfo,
	SigningKeyInfo,
} from "@/lib/types";

export async function listSigningKeys(): Promise<{ keys: SigningKeyInfo[] }> {
	return apiClient.get<{ keys: SigningKeyInfo[] }>("/v1/signing-keys");
}

export async function getSigningKey(
	keyId: string,
): Promise<SigningKeyDetailInfo> {
	return apiClient.get<SigningKeyDetailInfo>(
		`/v1/signing-keys/${encodeURIComponent(keyId)}`,
	);
}

export async function createSigningKey(
	data: CreateSigningKeyRequest,
): Promise<CreateSigningKeyResponse> {
	return apiClient.post<CreateSigningKeyResponse>("/v1/signing-keys", data);
}

export async function revokeSigningKey(keyId: string): Promise<void> {
	await apiClient.delete(`/v1/signing-keys/${encodeURIComponent(keyId)}`);
}
