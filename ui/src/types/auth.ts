/**
 * Authentication types for Thalamus Admin Panel
 *
 * These types mirror the backend auth endpoints:
 * - POST /v1/auth/oauth/{provider}/callback -> OAuthCallbackResponse
 * - POST /v1/auth/token/refresh -> TokenResponse
 * - GET /v1/auth/whoami -> CurrentUser
 */

/** User information from the backend */
export interface User {
	/** Unique user identifier */
	userId: string;
	/** Team identifier */
	teamId: string;
	/** Key identifier (if authenticated via API key) */
	keyId?: string;
	/** Token identifier (if authenticated via PASETO) */
	tokenId?: string;
	/** Granted scopes */
	scopes: string[];
	/** Granted roles */
	roles: string[];
}

/** OAuth callback response after successful OAuth flow */
export interface OAuthCallbackResponse {
	/** PASETO token for session */
	token: string;
	/** User ID */
	user_id: string;
	/** Team ID */
	team_id: string;
	/** Whether this is a new user */
	is_new_user: boolean;
}

/** Token refresh response */
export interface TokenResponse {
	/** Short-lived access token */
	access_token: string;
	/** Refresh token for obtaining new access tokens */
	refresh_token: string;
	/** Access token expiration in seconds */
	expires_in: number;
	/** Token type (Bearer) */
	token_type: string;
}

/** Login credentials for OPAQUE authentication */
export interface LoginCredentials {
	/** User email/username */
	email: string;
	/** User password */
	password: string;
}

/** OAuth provider information */
export interface OAuthProvider {
	/** Provider display name */
	name: string;
	/** Provider type (e.g., "google", "github") */
	provider_type: string;
}

/** OAuth login initiation response */
export interface OAuthLoginResponse {
	/** URL to redirect user to for authorization */
	authorization_url: string;
	/** State token for CSRF protection */
	state: string;
}

/** Authentication state */
export interface AuthState {
	/** Current user (null if not authenticated) */
	user: User | null;
	/** Whether the user is currently authenticated */
	isAuthenticated: boolean;
	/** Whether auth state is still loading */
	isLoading: boolean;
	/** Access token (short-lived) */
	accessToken: string | null;
	/** Refresh token */
	refreshToken: string | null;
	/** Token expiration timestamp */
	tokenExpiresAt: number | null;
}

/** Login function parameters */
export interface LoginParams {
	/** OAuth provider to use */
	provider?: string;
	/** Optional redirect URL after OAuth flow */
	redirectUrl?: string;
}

/** Auth context value */
export interface AuthContextValue {
	/** Current auth state */
	authState: AuthState;
	/** Login with OAuth */
	login: (params: LoginParams) => Promise<void>;
	/** Login with credentials (OPAQUE) */
	loginWithCredentials: (credentials: LoginCredentials) => Promise<void>;
	/** Logout and clear tokens */
	logout: () => Promise<void>;
	/** Refresh the access token */
	refreshToken: () => Promise<void>;
	/** Update user data */
	updateUser: (user: User) => void;
}
