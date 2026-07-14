import { apiClient } from "@/lib/api-client";
import type {
	ApiKeyInfo,
	CreateApiKeyRequest,
	CreateApiKeyResponse,
} from "@/lib/types";

export async function listApiKeys(): Promise<ApiKeyInfo[]> {
	return apiClient.get<ApiKeyInfo[]>("/v1/api-keys");
}

export async function createApiKey(
	data: CreateApiKeyRequest,
): Promise<CreateApiKeyResponse> {
	return apiClient.post<CreateApiKeyResponse>("/v1/api-keys", data);
}

export async function revokeApiKey(keyId: string): Promise<void> {
	await apiClient.post("/v1/api-keys/revoke", { key_id: keyId });
}

export async function rotateApiKey(
	keyId: string,
	gracePeriodMinutes?: number,
	reason?: string,
): Promise<void> {
	await apiClient.post("/v1/api-keys/rotate", {
		key_id: keyId,
		grace_period_minutes: gracePeriodMinutes,
		reason,
	});
}
