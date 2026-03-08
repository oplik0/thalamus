/**
 * Secure token storage using expo-secure-store
 *
 * Provides secure storage for:
 * - Access tokens (short-lived PASETO tokens)
 * - Refresh tokens (for token rotation)
 * - Token expiration timestamps
 */

import * as SecureStore from "expo-secure-store";

const KEYS = {
	ACCESS_TOKEN: "thalamus_access_token",
	REFRESH_TOKEN: "thalamus_refresh_token",
	TOKEN_EXPIRES_AT: "thalamus_token_expires_at",
} as const;

/**
 * Save the access token securely
 */
export async function saveAccessToken(token: string): Promise<void> {
	await SecureStore.setItemAsync(KEYS.ACCESS_TOKEN, token, {
		keychainService: "com.thalamus.admin",
	});
}

/**
 * Get the stored access token
 */
export async function getAccessToken(): Promise<string | null> {
	return SecureStore.getItemAsync(KEYS.ACCESS_TOKEN, {
		keychainService: "com.thalamus.admin",
	});
}

/**
 * Save the refresh token securely
 */
export async function saveRefreshToken(token: string): Promise<void> {
	await SecureStore.setItemAsync(KEYS.REFRESH_TOKEN, token, {
		keychainService: "com.thalamus.admin",
	});
}

/**
 * Get the stored refresh token
 */
export async function getRefreshToken(): Promise<string | null> {
	return SecureStore.getItemAsync(KEYS.REFRESH_TOKEN, {
		keychainService: "com.thalamus.admin",
	});
}

/**
 * Save the token expiration timestamp
 */
export async function saveTokenExpiresAt(timestamp: number): Promise<void> {
	await SecureStore.setItemAsync(
		KEYS.TOKEN_EXPIRES_AT,
		timestamp.toString(),
		{
			keychainService: "com.thalamus.admin",
		},
	);
}

/**
 * Get the stored token expiration timestamp
 */
export async function getTokenExpiresAt(): Promise<number | null> {
	const value = await SecureStore.getItemAsync(KEYS.TOKEN_EXPIRES_AT, {
		keychainService: "com.thalamus.admin",
	});
	return value ? parseInt(value, 10) : null;
}

/**
 * Check if the access token is expired or about to expire
 * @param bufferSeconds Additional buffer time (default 60 seconds)
 */
export async function isTokenExpired(bufferSeconds: number = 60): Promise<boolean> {
	const expiresAt = await getTokenExpiresAt();
	if (!expiresAt) return true;
	return Date.now() >= expiresAt * 1000 - bufferSeconds * 1000;
}

/**
 * Clear all stored tokens
 */
export async function clearTokens(): Promise<void> {
	await SecureStore.deleteItemAsync(KEYS.ACCESS_TOKEN, {
		keychainService: "com.thalamus.admin",
	});
	await SecureStore.deleteItemAsync(KEYS.REFRESH_TOKEN, {
		keychainService: "com.thalamus.admin",
	});
	await SecureStore.deleteItemAsync(KEYS.TOKEN_EXPIRES_AT, {
		keychainService: "com.thalamus.admin",
	});
}

/**
 * Save all auth tokens at once
 */
export async function saveTokens(
	accessToken: string,
	refreshToken: string,
	expiresIn: number,
): Promise<void> {
	await Promise.all([
		saveAccessToken(accessToken),
		saveRefreshToken(refreshToken),
		saveTokenExpiresAt(Math.floor(Date.now() / 1000) + expiresIn),
	]);
}

/**
 * Get all stored tokens
 */
export async function getTokens(): Promise<{
	accessToken: string | null;
	refreshToken: string | null;
	expiresAt: number | null;
}> {
	const [accessToken, refreshToken, expiresAtStr] = await Promise.all([
		getAccessToken(),
		getRefreshToken(),
		SecureStore.getItemAsync(KEYS.TOKEN_EXPIRES_AT, {
			keychainService: "com.thalamus.admin",
		}),
	]);

	return {
		accessToken,
		refreshToken,
		expiresAt: expiresAtStr ? parseInt(expiresAtStr, 10) : null,
	};
}
