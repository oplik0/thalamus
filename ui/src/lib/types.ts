/**
 * TypeScript types matching backend DTOs
 * These types correspond to the Rust backend's actual request/response shapes.
 */

// ─── Auth Types ────────────────────────────────────────────

export interface WhoamiResponse {
	user_id: string;
	team_id: string;
	key_id?: string;
	token_id?: string;
	scopes?: string[];
	roles?: string[];
}

export interface TokenResponse {
	token: string;
	expires_in: number;
}

export interface TokenRefreshResponse {
	access_token: string;
	refresh_token: string;
	expires_in: number;
	token_type: string;
}

export interface OAuthProviderInfo {
	name: string;
	provider_type: string;
}

export interface OAuthLoginResponse {
	authorization_url: string;
	state: string;
}

export interface OAuthCallbackResponse {
	token: string;
	user_id: string;
	team_id: string;
	is_new_user: boolean;
}

export interface LoginStartResponse {
	message: string;
	server_state: string;
}

export interface SetupResponse {
	token: string;
	user_id: string;
	team_id: string;
}

// ─── API Key Types ─────────────────────────────────────────

export interface ApiKeyInfo {
	id: string;
	key_prefix: string;
	name: string;
	description?: string;
	scopes?: string[];
	is_active: boolean;
	last_used_at?: string;
	expires_at?: string;
	created_at: string;
}

export interface CreateApiKeyRequest {
	name: string;
	description?: string;
	scopes?: string[];
	expires_in_days?: number;
}

export interface CreateApiKeyResponse {
	id: string;
	key: string;
	key_prefix: string;
	name: string;
	scopes?: string[];
	created_at: string;
	expires_at?: string;
}

export interface RevokeKeyRequest {
	key_id: string;
}

export interface RotateKeyRequest {
	key_id: string;
	grace_period_minutes?: number;
	reason?: string;
}

// ─── Signing Key Types ─────────────────────────────────────

export type SigningAlgorithm =
	| "RS256"
	| "RS384"
	| "RS512"
	| "ES256"
	| "ES384"
	| "ES512"
	| "Ed25519";

export interface SigningKeyInfo {
	id: string;
	key_id: string;
	algorithm: string;
	fingerprint: string;
	name?: string;
	scopes?: string[];
	is_active: boolean;
	expires_at?: string;
	last_used_at?: string;
	use_count: number;
	created_at: string;
}

export interface SigningKeyDetailInfo extends SigningKeyInfo {
	public_key: string;
}

export interface CreateSigningKeyRequest {
	algorithm: string;
	name?: string;
	description?: string;
	scopes?: string[];
	expires_in_days?: number;
}

export interface CreateSigningKeyResponse {
	key_id: string;
	private_key: string;
	public_key: string;
	algorithm: string;
	fingerprint: string;
	name?: string;
	scopes?: string[];
	expires_at?: string;
	warning: string;
}

// ─── Refresh Token Types ───────────────────────────────────

export interface RefreshTokenInfo {
	id: string;
	family: string;
	scopes?: string[];
	roles?: string[];
	expires_at: string;
	is_active: boolean;
	revoked_at?: string;
}

export interface CreateRefreshTokenRequest {
	expires_in_days?: number;
}

export interface RefreshTokenResponse {
	refresh_token: string;
	expires_at: string;
}

// ─── Authorization Types ───────────────────────────────────

export interface PolicyInfo {
	subject: string;
	domain: string;
	object: string;
	action: string;
}

export interface CreatePolicyRequest {
	subject: string;
	domain: string;
	object: string;
	action: string;
}

export interface PoliciesListResponse {
	policies: PolicyInfo[];
}

export interface CreateRoleRequest {
	user: string;
	role: string;
	domain: string;
}

export interface RoleResponse {
	user: string;
	role: string;
	domain: string;
}

export interface RolesListResponse {
	user: string;
	domain: string;
	roles: string[];
}

export interface RemoveRoleRequest {
	role: string;
}

// ─── Health Types ──────────────────────────────────────────

export interface HealthResponse {
	status: string;
	version: string;
}

// ─── Team Types ────────────────────────────────────────────

export interface Team {
	id: string;
	name: string;
	slug?: string;
	description?: string;
	parent_team_id?: string;
	is_active: boolean;
	created_at: string;
	updated_at: string;
}

export interface TeamMember {
	id: string;
	user_id: string;
	username: string;
	email: string;
	role: string;
	created_at: string;
}

export interface Project {
	id: string;
	team_id: string;
	name: string;
	description?: string;
	metadata?: Record<string, unknown>;
	created_at: string;
	updated_at: string;
}

export interface CreateTeamRequest {
	name: string;
	description?: string;
	parent_team_id?: string;
}

export interface UpdateTeamRequest {
	name?: string;
	description?: string;
	is_active?: boolean;
}

export interface AddMemberRequest {
	user_id: string;
	role: string;
}

export interface UpdateMemberRoleRequest {
	role: string;
}

export interface CreateProjectRequest {
	name: string;
	description?: string;
	metadata?: Record<string, unknown>;
}

export interface UpdateProjectRequest {
	name?: string;
	description?: string;
	metadata?: Record<string, unknown>;
}

export interface SetParentRequest {
	parent_team_id?: string;
}

// ─── API Error ─────────────────────────────────────────────

export interface ApiErrorResponse {
	code?: string;
	message: string;
	details?: Record<string, unknown>;
}
