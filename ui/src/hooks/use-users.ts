import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { ChangePasswordRequest, CreateUserRequest } from "@/lib/types";
import {
	changePassword,
	createUser,
	getUser,
	listUsers,
} from "@/services/users";

export function useUsers() {
	return useQuery({
		queryKey: ["users"],
		queryFn: listUsers,
	});
}

export function useUser(userId: string) {
	return useQuery({
		queryKey: ["users", userId],
		queryFn: () => getUser(userId),
		enabled: !!userId,
	});
}

export function useCreateUser() {
	const queryClient = useQueryClient();

	return useMutation({
		mutationFn: (data: CreateUserRequest) => createUser(data),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["users"] });
		},
	});
}

export function useChangePassword() {
	return useMutation({
		mutationFn: (data: ChangePasswordRequest) => changePassword(data),
	});
}
