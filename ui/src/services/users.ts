import { apiClient } from "@/lib/api-client";
import { ensureOpaqueReady, opaque } from "@/lib/opaque";
import type {
	ChangePasswordRequest,
	CreateUserRequest,
	CreateUserResponse,
	UserInfo,
} from "@/lib/types";

interface RegistrationStartResponse {
	message: string;
}

async function createRegistrationRecord(
	password: string,
	start: (registrationRequest: string) => Promise<RegistrationStartResponse>,
): Promise<string> {
	await ensureOpaqueReady();

	const { clientRegistrationState, registrationRequest } =
		opaque.client.startRegistration({ password });

	const { message: registrationResponse } = await start(registrationRequest);
	const { registrationRecord } = opaque.client.finishRegistration({
		clientRegistrationState,
		registrationResponse,
		password,
	});

	return registrationRecord;
}

export async function listUsers(): Promise<UserInfo[]> {
	return apiClient.get<UserInfo[]>("/v1/users");
}

export async function getUser(userId: string): Promise<UserInfo> {
	return apiClient.get<UserInfo>(`/v1/users/${encodeURIComponent(userId)}`);
}

export async function createUser(
	data: CreateUserRequest,
): Promise<CreateUserResponse> {
	const message = await createRegistrationRecord(
		data.password,
		(registrationRequest) =>
			apiClient.post<RegistrationStartResponse>("/v1/users/register/start", {
				username: data.username,
				email: data.email,
				message: registrationRequest,
			}),
	);

	return apiClient.post<CreateUserResponse>("/v1/users/register/finish", {
		username: data.username,
		email: data.email,
		message,
		team_id: data.team_id,
		role: data.role,
	});
}

export async function changePassword(
	data: ChangePasswordRequest,
): Promise<void> {
	const message = await createRegistrationRecord(
		data.password,
		(registrationRequest) =>
			apiClient.post<RegistrationStartResponse>("/v1/users/me/password/start", {
				message: registrationRequest,
			}),
	);

	await apiClient.post("/v1/users/me/password/finish", { message });
}
