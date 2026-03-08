"use client";

import React, {
	createContext,
	useContext,
	useEffect,
	useState,
	useCallback,
	ReactNode,
} from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { WhoamiResponse } from "@/lib/types";
import { whoami, logout as logoutService, refreshToken } from "@/services/auth";
import { clearToken, getToken } from "@/lib/auth";
import { isAuthError } from "@/services/auth";

interface AuthContextType {
	user: WhoamiResponse | null;
	isAuthenticated: boolean;
	isLoading: boolean;
	logout: () => Promise<void>;
	refetchUser: () => Promise<void>;
}

const AuthContext = createContext<AuthContextType | undefined>(undefined);

interface AuthProviderProps {
	children: ReactNode;
}

export function AuthProvider({ children }: AuthProviderProps) {
	const queryClient = useQueryClient();
	const [isLoading, setIsLoading] = useState(true);

	const {
		data: user,
		isLoading: isQueryLoading,
		isError,
		refetch,
	} = useQuery({
		queryKey: ["whoami"],
		queryFn: whoami,
		retry: false,
		staleTime: 5 * 60 * 1000, // 5 minutes
		enabled: false, // Don't auto-fetch, we'll enable after initial token check
	});

	// Check for token on mount and enable fetching
	useEffect(() => {
		const initAuth = async () => {
			const token = await getToken();
			if (token) {
				// Enable the query to fetch user data
				try {
					await queryClient.fetchQuery({ queryKey: ["whoami"], queryFn: whoami });
				} catch (error) {
					// Token might be invalid/expired
					if (isAuthError(error)) {
						// Try to refresh the token
						try {
							await refreshToken();
							await queryClient.fetchQuery({ queryKey: ["whoami"], queryFn: whoami });
						} catch {
							// Refresh failed, clear tokens
							await clearToken();
						}
					}
				}
			}
			setIsLoading(false);
		};
		initAuth();
	}, [queryClient]);

	const handleLogout = useCallback(async () => {
		await logoutService();
		queryClient.clear();
	}, [queryClient]);

	const handleRefetchUser = useCallback(async () => {
		await refetch();
	}, [refetch]);

	// Combined loading state
	const isAuthLoading = isLoading || isQueryLoading;

	return (
		<AuthContext.Provider
			value={{
				user: user ?? null,
				isAuthenticated: !!user && !isError,
				isLoading: isAuthLoading,
				logout: handleLogout,
				refetchUser: handleRefetchUser,
			}}
		>
			{children}
		</AuthContext.Provider>
	);
}

export function useAuth(): AuthContextType {
	const context = useContext(AuthContext);
	if (context === undefined) {
		throw new Error("useAuth must be used within an AuthProvider");
	}
	return context;
}
