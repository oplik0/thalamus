import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type {
	AddMemberRequest,
	CreateProjectRequest,
	CreateTeamRequest,
	UpdateMemberRoleRequest,
	UpdateProjectRequest,
	UpdateTeamRequest,
} from "@/lib/types";
import {
	addMember,
	createProject,
	createTeam,
	deleteProject,
	deleteTeam,
	getTeam,
	listMembers,
	listProjects,
	listTeams,
	removeMember,
	updateMemberRole,
	updateProject,
	updateTeam,
} from "@/services/teams";

// ─── Teams ─────────────────────────────────────────────────

export function useTeams() {
	return useQuery({
		queryKey: ["teams"],
		queryFn: listTeams,
	});
}

export function useTeam(teamId: string) {
	return useQuery({
		queryKey: ["teams", teamId],
		queryFn: () => getTeam(teamId),
		enabled: !!teamId,
	});
}

export function useCreateTeam() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (data: CreateTeamRequest) => createTeam(data),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["teams"] });
		},
	});
}

export function useUpdateTeam(teamId: string) {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (data: UpdateTeamRequest) => updateTeam(teamId, data),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["teams"] });
			queryClient.invalidateQueries({ queryKey: ["teams", teamId] });
		},
	});
}

export function useDeleteTeam() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (teamId: string) => deleteTeam(teamId),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["teams"] });
		},
	});
}

// ─── Members ───────────────────────────────────────────────

export function useTeamMembers(teamId: string) {
	return useQuery({
		queryKey: ["teams", teamId, "members"],
		queryFn: () => listMembers(teamId),
		enabled: !!teamId,
	});
}

export function useAddMember(teamId: string) {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (data: AddMemberRequest) => addMember(teamId, data),
		onSuccess: () => {
			queryClient.invalidateQueries({
				queryKey: ["teams", teamId, "members"],
			});
		},
	});
}

export function useUpdateMemberRole(teamId: string) {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: ({
			userId,
			data,
		}: {
			userId: string;
			data: UpdateMemberRoleRequest;
		}) => updateMemberRole(teamId, userId, data),
		onSuccess: () => {
			queryClient.invalidateQueries({
				queryKey: ["teams", teamId, "members"],
			});
		},
	});
}

export function useRemoveMember(teamId: string) {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (userId: string) => removeMember(teamId, userId),
		onSuccess: () => {
			queryClient.invalidateQueries({
				queryKey: ["teams", teamId, "members"],
			});
		},
	});
}

// ─── Projects ──────────────────────────────────────────────

export function useTeamProjects(teamId: string) {
	return useQuery({
		queryKey: ["teams", teamId, "projects"],
		queryFn: () => listProjects(teamId),
		enabled: !!teamId,
	});
}

export function useCreateProject(teamId: string) {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (data: CreateProjectRequest) => createProject(teamId, data),
		onSuccess: () => {
			queryClient.invalidateQueries({
				queryKey: ["teams", teamId, "projects"],
			});
		},
	});
}

export function useUpdateProject(teamId: string) {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: ({
			projectId,
			data,
		}: {
			projectId: string;
			data: UpdateProjectRequest;
		}) => updateProject(teamId, projectId, data),
		onSuccess: () => {
			queryClient.invalidateQueries({
				queryKey: ["teams", teamId, "projects"],
			});
		},
	});
}

export function useDeleteProject(teamId: string) {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (projectId: string) => deleteProject(teamId, projectId),
		onSuccess: () => {
			queryClient.invalidateQueries({
				queryKey: ["teams", teamId, "projects"],
			});
		},
	});
}
