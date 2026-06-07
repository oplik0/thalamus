import { useQuery } from "@tanstack/react-query";
import { apiClient } from "@/lib/api-client";
import type { HealthResponse } from "@/lib/types";

export function useHealthCheck() {
	return useQuery({
		queryKey: ["health"],
		queryFn: () => apiClient.get<HealthResponse>("/health"),
		refetchInterval: 30_000,
	});
}
