import * as Linking from "expo-linking";
import { ApiError, apiClient } from "@/lib/api-client";
import {
	clearToken,
	getRefreshToken,
	setRefreshToken,
	setToken,
} from "@/lib/auth";
import { ensureOpaqueReady, opaque } from "@/lib/opaque";
import type {
	LoginStartResponse,
	OAuthCallbackResponse,
	OAuthProviderInfo,
	RefreshTokenInfo,
	SetupResponse,
	TokenRefreshResponse,
	WhoamiResponse,
} from "@/lib/types";

export async function getProviders(): Promise<OAuthProviderInfo[]> {
	return apiClient.get<OAuthProviderInfo[]>("/v1/auth/oauth/providers");
}

/**
 * Check whether first-run setup is required.
 */
export async function getSetupStatus(): Promise<{ needs_setup: boolean }> {
	return apiClient.get<{ needs_setup: boolean }>("/v1/auth/setup-status");
}

/**
 * Start OAuth flow
 * 1. Get authorization URL from backend (with frontend callback as redirect)
 * 2. Redirect to OAuth provider for authentication
 * 3. OAuth provider redirects to frontend callback URL with token
 * 4. Frontend stores token and redirects to dashboard
 */
export async function startOAuthFlow(
	providerName: string,
): Promise<OAuthCallbackResponse> {
	// Determine the callback URL based on environment
	let frontendCallbackUrl: string;

	if (typeof window !== "undefined" && window.location.protocol !== "file:") {
		// Web: use the current origin + callback path
		frontendCallbackUrl = `${window.location.origin}/login/oauth/callback`;
	} else {
		// Native: use expo-linking to get the scheme-based URL
		frontendCallbackUrl = Linking.createURL("login/oauth/callback");
	}

	// Get the authorization URL from our backend
	// Pass the frontend's callback URL as the redirect target
	const { authorization_url } = await apiClient.get<{
		authorization_url: string;
		state: string;
	}>(
		`/v1/auth/oauth/${encodeURIComponent(
			providerName,
		)}/login?redirect_url=${encodeURIComponent(frontendCallbackUrl)}`,
	);

	// Redirect to the OAuth provider - this replaces the current page
	// After auth, the provider will redirect back to our callback URL
	window.location.href = authorization_url;

	// This won't be reached since the page redirects away
	throw new Error("Redirecting to OAuth provider...");
}

/**
 * Handle OAuth callback - call backend to exchange code for token
 * This is called from the frontend's callback page
 */
export async function handleOAuthCallback(
	providerName: string,
	code: string,
	state: string,
): Promise<OAuthCallbackResponse> {
	const response = await apiClient.get<OAuthCallbackResponse>(
		`/v1/auth/oauth/${encodeURIComponent(
			providerName,
		)}/callback?code=${encodeURIComponent(code)}&state=${encodeURIComponent(state)}`,
	);
	await setToken(response.token);
	return response;
}

/**
 * Get current user info
 */
export async function whoami(): Promise<WhoamiResponse> {
	return apiClient.get<WhoamiResponse>("/v1/auth/whoami");
}

/**
 * Exchange API key auth for a short-lived PASETO token
 */
export async function exchangeToken(): Promise<{
	token: string;
	expires_in: number;
}> {
	return apiClient.post<{ token: string; expires_in: number }>(
		"/v1/auth/token",
	);
}

/**
 * Refresh the access token
 */
export async function refreshToken(): Promise<TokenRefreshResponse> {
	const refreshTokenValue = await getRefreshToken();
	if (!refreshTokenValue) {
		throw new Error("No refresh token available");
	}
	const response = await apiClient.post<TokenRefreshResponse>(
		"/v1/auth/token/refresh",
		{ refresh_token: refreshTokenValue },
	);
	await setToken(response.access_token);
	if (response.refresh_token) {
		await setRefreshToken(response.refresh_token);
	}
	return response;
}

/**
 * Logout - revoke token and clear storage
 */
export async function logout(): Promise<void> {
	try {
		await apiClient.post("/v1/auth/logout");
	} catch {
		// Ignore errors - clear local state regardless
	}
	await clearToken();
}

/**
 * List refresh tokens for current user
 */
export async function listRefreshTokens(): Promise<{
	tokens: RefreshTokenInfo[];
}> {
	return apiClient.get<{ tokens: RefreshTokenInfo[] }>(
		"/v1/auth/refresh-tokens",
	);
}

/**
 * Revoke a specific refresh token
 */
export async function revokeRefreshToken(tokenId: string): Promise<void> {
	await apiClient.post("/v1/auth/refresh-tokens/revoke", {
		token_id: tokenId,
	});
}

/**
 * Log in with username and password using OPAQUE.
 */
export async function loginWithCredentials(
	username: string,
	password: string,
): Promise<void> {
	await ensureOpaqueReady();

	const { clientLoginState, startLoginRequest } = opaque.client.startLogin({
		password,
	});

	const { message: loginResponse, server_state } =
		await apiClient.post<LoginStartResponse>("/v1/auth/login/start", {
			username,
			message: startLoginRequest,
		});

	const loginResult = opaque.client.finishLogin({
		clientLoginState,
		loginResponse,
		password,
	});

	if (!loginResult) {
		throw new Error("Invalid username or password");
	}

	const { finishLoginRequest } = loginResult;

	const { token } = await apiClient.post<{ token: string }>(
		"/v1/auth/login/finish",
		{
			username,
			finish_login_request: finishLoginRequest,
			server_state,
		},
	);

	await setToken(token);
}

/**
 * Perform first-run setup by creating the first admin user with an OPAQUE
 * password. The actual OPAQUE registration is run server-side because there is
 * no prior authentication at this point.
 */
export async function setupAccount(
	username: string,
	email: string,
	password: string,
): Promise<SetupResponse> {
	const response = await apiClient.post<SetupResponse>("/v1/auth/setup", {
		username,
		email,
		password,
	});
	await setToken(response.token);
	return response;
}

/**
 * Check if an error is an authentication error (401/403)
 */
export function isAuthError(error: unknown): boolean {
	if (error instanceof ApiError) {
		return error.status === 401 || error.status === 403;
	}
	return false;
}
