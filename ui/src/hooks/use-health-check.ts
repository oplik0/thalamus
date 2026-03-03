import { useQuery } from "@tanstack/react-query";
import { apiClient } from "@/lib/api-client";

export interface HealthResponse {
	status: string;
}

export function useHealthCheck() {
	return useQuery({
		queryKey: ["health"],
		queryFn: () => apiClient.get<HealthResponse>("/health"),
		refetchInterval: 30_000, // Poll every 30 seconds
	});
}
