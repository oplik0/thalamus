import { QueryClient } from "@tanstack/react-query";

export const queryClient = new QueryClient({
	defaultOptions: {
		queries: {
			// Refetch on window focus for fresh data
			refetchOnWindowFocus: true,
			// Retry failed requests up to 2 times
			retry: 2,
			// Cache data for 5 minutes
			staleTime: 5 * 60 * 1000,
		},
		mutations: {
			retry: 1,
		},
	},
});
