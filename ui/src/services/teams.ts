import { apiClient } from "@/lib/api-client";
import type {
	AddMemberRequest,
	CreateProjectRequest,
	CreateTeamRequest,
	Project,
	SetParentRequest,
	Team,
	TeamMember,
	UpdateMemberRoleRequest,
	UpdateProjectRequest,
	UpdateTeamRequest,
} from "@/lib/types";

// ─── Teams ─────────────────────────────────────────────────

export async function listTeams(): Promise<Team[]> {
	return apiClient.get<Team[]>("/v1/teams");
}

export async function getTeam(teamId: string): Promise<Team> {
	return apiClient.get<Team>(`/v1/teams/${encodeURIComponent(teamId)}`);
}

export async function createTeam(data: CreateTeamRequest): Promise<Team> {
	return apiClient.post<Team>("/v1/teams", data);
}

export async function updateTeam(
	teamId: string,
	data: UpdateTeamRequest,
): Promise<Team> {
	return apiClient.put<Team>(`/v1/teams/${encodeURIComponent(teamId)}`, data);
}

export async function deleteTeam(teamId: string): Promise<void> {
	await apiClient.delete(`/v1/teams/${encodeURIComponent(teamId)}`);
}

export async function setTeamParent(
	teamId: string,
	data: SetParentRequest,
): Promise<Team> {
	return apiClient.put<Team>(
		`/v1/teams/${encodeURIComponent(teamId)}/parent`,
		data,
	);
}

export async function removeTeamParent(teamId: string): Promise<Team> {
	return apiClient.delete<Team>(
		`/v1/teams/${encodeURIComponent(teamId)}/parent`,
	);
}

// ─── Members ───────────────────────────────────────────────

export async function listMembers(teamId: string): Promise<TeamMember[]> {
	return apiClient.get<TeamMember[]>(
		`/v1/teams/${encodeURIComponent(teamId)}/members`,
	);
}

export async function addMember(
	teamId: string,
	data: AddMemberRequest,
): Promise<TeamMember> {
	return apiClient.post<TeamMember>(
		`/v1/teams/${encodeURIComponent(teamId)}/members`,
		data,
	);
}

export async function updateMemberRole(
	teamId: string,
	userId: string,
	data: UpdateMemberRoleRequest,
): Promise<TeamMember> {
	return apiClient.put<TeamMember>(
		`/v1/teams/${encodeURIComponent(teamId)}/members/${encodeURIComponent(userId)}`,
		data,
	);
}

export async function removeMember(
	teamId: string,
	userId: string,
): Promise<void> {
	await apiClient.delete(
		`/v1/teams/${encodeURIComponent(teamId)}/members/${encodeURIComponent(userId)}`,
	);
}

// ─── Projects ──────────────────────────────────────────────

export async function listProjects(teamId: string): Promise<Project[]> {
	return apiClient.get<Project[]>(
		`/v1/teams/${encodeURIComponent(teamId)}/projects`,
	);
}

export async function getProject(
	teamId: string,
	projectId: string,
): Promise<Project> {
	return apiClient.get<Project>(
		`/v1/teams/${encodeURIComponent(teamId)}/projects/${encodeURIComponent(projectId)}`,
	);
}

export async function createProject(
	teamId: string,
	data: CreateProjectRequest,
): Promise<Project> {
	return apiClient.post<Project>(
		`/v1/teams/${encodeURIComponent(teamId)}/projects`,
		data,
	);
}

export async function updateProject(
	teamId: string,
	projectId: string,
	data: UpdateProjectRequest,
): Promise<Project> {
	return apiClient.put<Project>(
		`/v1/teams/${encodeURIComponent(teamId)}/projects/${encodeURIComponent(projectId)}`,
		data,
	);
}

export async function deleteProject(
	teamId: string,
	projectId: string,
): Promise<void> {
	await apiClient.delete(
		`/v1/teams/${encodeURIComponent(teamId)}/projects/${encodeURIComponent(projectId)}`,
	);
}
