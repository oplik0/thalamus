"use client";

import { Link, useLocalSearchParams, useRouter } from "expo-router";
import {
	ChevronDown,
	ChevronLeft,
	FolderKanban,
	Pencil,
	Plus,
	Shield,
	Trash2,
	User,
	Users,
} from "lucide-react-native";
import { useState } from "react";
import { ScrollView, View } from "react-native";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { EmptyState } from "@/components/empty-state";
import { Alert, AlertText } from "@/components/ui/alert";
import { Badge, BadgeText } from "@/components/ui/badge";
import { Box } from "@/components/ui/box";
import {
	Button,
	ButtonIcon,
	ButtonSpinner,
	ButtonText,
} from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Divider } from "@/components/ui/divider";
import {
	FormControl,
	FormControlHelper,
	FormControlHelperText,
	FormControlLabel,
	FormControlLabelText,
} from "@/components/ui/form-control";
import { Heading } from "@/components/ui/heading";
import { HStack } from "@/components/ui/hstack";
import { Input, InputField } from "@/components/ui/input";
import {
	Modal,
	ModalBackdrop,
	ModalBody,
	ModalContent,
	ModalFooter,
	ModalHeader,
} from "@/components/ui/modal";
import {
	Select,
	SelectBackdrop,
	SelectContent,
	SelectDragIndicator,
	SelectDragIndicatorWrapper,
	SelectIcon,
	SelectInput,
	SelectItem,
	SelectPortal,
	SelectTrigger,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { Textarea, TextareaInput } from "@/components/ui/textarea";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { VStack } from "@/components/ui/vstack";
import {
	useAddMember,
	useCreateProject,
	useDeleteProject,
	useDeleteTeam,
	useRemoveMember,
	useTeam,
	useTeamMembers,
	useTeamProjects,
	useUpdateMemberRole,
	useUpdateProject,
	useUpdateTeam,
} from "@/hooks/use-teams";
import type { Project, Team, TeamMember } from "@/lib/types";

const TEAM_ROLES = [
	{ value: "team_admin", label: "Admin" },
	{ value: "team_member", label: "Member" },
	{ value: "team_readonly", label: "Read-only" },
];

function RoleBadge({ role }: { role: string }) {
	const config =
		role === "team_admin"
			? { action: "warning" as const, label: "Admin" }
			: role === "team_readonly"
				? { action: "muted" as const, label: "Read-only" }
				: { action: "info" as const, label: "Member" };

	return (
		<Badge action={config.action} variant="outline" size="sm">
			<BadgeText>{config.label}</BadgeText>
		</Badge>
	);
}

// ─── Team Header / Edit / Delete ───────────────────────────

function TeamHeader({
	team,
	onEdit,
	onDelete,
}: {
	team: Team;
	onEdit: () => void;
	onDelete: () => void;
}) {
	return (
		<Card className="p-5 gap-3">
			<HStack className="justify-between items-start">
				<VStack className="flex-1 gap-1">
					<HStack className="items-center gap-2">
						<Users size={22} className="text-primary-500" />
						<Heading size="lg">{team.name}</Heading>
						{!team.is_active && (
							<Badge action="muted" variant="outline" size="sm">
								<BadgeText>Inactive</BadgeText>
							</Badge>
						)}
					</HStack>
					{team.description && (
						<Text size="sm" className="text-typography-500">
							{team.description}
						</Text>
					)}
				</VStack>
				<HStack className="gap-1">
					<Button
						size="xs"
						variant="outline"
						action="secondary"
						onPress={onEdit}
						accessibilityLabel="Edit team"
					>
						<ButtonIcon as={Pencil} />
					</Button>
					<Button
						size="xs"
						variant="outline"
						action="negative"
						onPress={onDelete}
						accessibilityLabel="Delete team"
					>
						<ButtonIcon as={Trash2} />
					</Button>
				</HStack>
			</HStack>

			<HStack className="gap-4 flex-wrap">
				<Text size="xs" className="text-typography-400 font-mono">
					ID: {team.id}
				</Text>
				{team.slug && (
					<Text size="xs" className="text-typography-400">
						Slug: {team.slug}
					</Text>
				)}
				{team.parent_team_id && (
					<Badge action="info" variant="outline" size="sm">
						<BadgeText>Parent: {team.parent_team_id}</BadgeText>
					</Badge>
				)}
			</HStack>
		</Card>
	);
}

// ─── Projects Section ──────────────────────────────────────

function ProjectRow({
	project,
	onEdit,
	onDelete,
}: {
	project: Project;
	onEdit: () => void;
	onDelete: () => void;
}) {
	return (
		<Card className="p-4">
			<HStack className="justify-between items-start">
				<VStack className="flex-1 gap-1">
					<HStack className="items-center gap-2">
						<FolderKanban size={16} className="text-primary-500" />
						<Text className="font-semibold">{project.name}</Text>
					</HStack>
					{project.description && (
						<Text size="sm" className="text-typography-500">
							{project.description}
						</Text>
					)}
					<Text size="xs" className="text-typography-400">
						Created {new Date(project.created_at).toLocaleDateString()}
					</Text>
				</VStack>
				<HStack className="gap-1">
					<Button
						size="xs"
						variant="outline"
						action="secondary"
						onPress={onEdit}
						accessibilityLabel="Edit project"
					>
						<ButtonIcon as={Pencil} />
					</Button>
					<Button
						size="xs"
						variant="outline"
						action="negative"
						onPress={onDelete}
						accessibilityLabel="Delete project"
					>
						<ButtonIcon as={Trash2} />
					</Button>
				</HStack>
			</HStack>
		</Card>
	);
}

// ─── Members Section ───────────────────────────────────────

function MemberRow({
	member,
	onEdit,
	onRemove,
}: {
	member: TeamMember;
	onEdit: () => void;
	onRemove: () => void;
}) {
	return (
		<Card className="p-4">
			<HStack className="justify-between items-center">
				<HStack className="items-center gap-3 flex-1">
					<View className="w-9 h-9 rounded-full bg-primary-100 items-center justify-center">
						<User size={16} className="text-primary-600" />
					</View>
					<VStack className="flex-1 gap-0.5">
						<HStack className="items-center gap-2 flex-wrap">
							<Text className="font-semibold">{member.username}</Text>
							<RoleBadge role={member.role} />
						</HStack>
						<Text size="xs" className="text-typography-500">
							{member.email}
						</Text>
					</VStack>
				</HStack>
				<HStack className="gap-1">
					<Button
						size="xs"
						variant="outline"
						action="secondary"
						onPress={onEdit}
						accessibilityLabel="Edit member role"
					>
						<ButtonIcon as={Pencil} />
					</Button>
					<Button
						size="xs"
						variant="outline"
						action="negative"
						onPress={onRemove}
						accessibilityLabel="Remove member"
					>
						<ButtonIcon as={Trash2} />
					</Button>
				</HStack>
			</HStack>
		</Card>
	);
}

export default function TeamDetailPage() {
	const { id } = useLocalSearchParams<{ id: string }>();
	const router = useRouter();
	const teamId = id;

	const { data: team, isLoading: isLoadingTeam } = useTeam(teamId);
	const { data: members, isLoading: isLoadingMembers } = useTeamMembers(teamId);
	const { data: projects, isLoading: isLoadingProjects } =
		useTeamProjects(teamId);

	const updateTeam = useUpdateTeam(teamId);
	const deleteTeam = useDeleteTeam();
	const addMember = useAddMember(teamId);
	const updateMemberRole = useUpdateMemberRole(teamId);
	const removeMember = useRemoveMember(teamId);
	const createProject = useCreateProject(teamId);
	const updateProject = useUpdateProject(teamId);
	const deleteProject = useDeleteProject(teamId);

	const toast = useToast();

	// Modals / dialogs
	const [showEditTeam, setShowEditTeam] = useState(false);
	const [showAddMember, setShowAddMember] = useState(false);
	const [showCreateProject, setShowCreateProject] = useState(false);
	const [editingMember, setEditingMember] = useState<TeamMember | null>(null);
	const [editingProject, setEditingProject] = useState<Project | null>(null);
	const [deleteTeamTarget, setDeleteTeamTarget] = useState<Team | null>(null);
	const [deleteProjectTarget, setDeleteProjectTarget] =
		useState<Project | null>(null);
	const [removeMemberTarget, setRemoveMemberTarget] =
		useState<TeamMember | null>(null);

	// Forms
	const [teamForm, setTeamForm] = useState({
		name: "",
		description: "",
		is_active: true,
	});
	const [memberForm, setMemberForm] = useState({ user_id: "", role: "" });
	const [projectForm, setProjectForm] = useState({
		name: "",
		description: "",
		metadata: "",
	});

	const resetTeamForm = (t: Team) => {
		setTeamForm({
			name: t.name,
			description: t.description ?? "",
			is_active: t.is_active,
		});
	};

	const resetMemberForm = () => {
		setMemberForm({ user_id: "", role: "team_member" });
	};

	const resetProjectForm = () => {
		setProjectForm({ name: "", description: "", metadata: "" });
	};

	const handleUpdateTeam = async () => {
		await updateTeam.mutateAsync({
			name: teamForm.name.trim(),
			description: teamForm.description.trim() || undefined,
			is_active: teamForm.is_active,
		});
		setShowEditTeam(false);
		toast.show({
			id: "team-updated",
			render: () => (
				<Toast action="success">
					<ToastTitle>Team updated</ToastTitle>
				</Toast>
			),
		});
	};

	const handleDeleteTeam = async () => {
		if (!deleteTeamTarget) return;
		await deleteTeam.mutateAsync(deleteTeamTarget.id);
		setDeleteTeamTarget(null);
		toast.show({
			id: "team-deleted",
			render: () => (
				<Toast action="success">
					<ToastTitle>Team deleted</ToastTitle>
				</Toast>
			),
		});
		router.replace("/(tabs)/(admin)/teams");
	};

	const handleAddMember = async () => {
		await addMember.mutateAsync({
			user_id: memberForm.user_id.trim(),
			role: memberForm.role,
		});
		setShowAddMember(false);
		resetMemberForm();
		toast.show({
			id: "member-added",
			render: () => (
				<Toast action="success">
					<ToastTitle>Member added</ToastTitle>
				</Toast>
			),
		});
	};

	const handleUpdateMemberRole = async () => {
		if (!editingMember) return;
		await updateMemberRole.mutateAsync({
			userId: editingMember.user_id,
			data: { role: memberForm.role },
		});
		setEditingMember(null);
		toast.show({
			id: "member-role-updated",
			render: () => (
				<Toast action="success">
					<ToastTitle>Member role updated</ToastTitle>
				</Toast>
			),
		});
	};

	const handleRemoveMember = async () => {
		if (!removeMemberTarget) return;
		await removeMember.mutateAsync(removeMemberTarget.user_id);
		setRemoveMemberTarget(null);
		toast.show({
			id: "member-removed",
			render: () => (
				<Toast action="success">
					<ToastTitle>Member removed</ToastTitle>
				</Toast>
			),
		});
	};

	const parseMetadata = (): Record<string, unknown> | undefined => {
		const raw = projectForm.metadata.trim();
		if (!raw) return undefined;
		try {
			return JSON.parse(raw) as Record<string, unknown>;
		} catch {
			return undefined;
		}
	};

	const handleCreateProject = async () => {
		await createProject.mutateAsync({
			name: projectForm.name.trim(),
			description: projectForm.description.trim() || undefined,
			metadata: parseMetadata(),
		});
		setShowCreateProject(false);
		resetProjectForm();
		toast.show({
			id: "project-created",
			render: () => (
				<Toast action="success">
					<ToastTitle>Project created</ToastTitle>
				</Toast>
			),
		});
	};

	const handleUpdateProject = async () => {
		if (!editingProject) return;
		await updateProject.mutateAsync({
			projectId: editingProject.id,
			data: {
				name: projectForm.name.trim(),
				description: projectForm.description.trim() || undefined,
				metadata: parseMetadata(),
			},
		});
		setEditingProject(null);
		toast.show({
			id: "project-updated",
			render: () => (
				<Toast action="success">
					<ToastTitle>Project updated</ToastTitle>
				</Toast>
			),
		});
	};

	const handleDeleteProject = async () => {
		if (!deleteProjectTarget) return;
		await deleteProject.mutateAsync(deleteProjectTarget.id);
		setDeleteProjectTarget(null);
		toast.show({
			id: "project-deleted",
			render: () => (
				<Toast action="success">
					<ToastTitle>Project deleted</ToastTitle>
				</Toast>
			),
		});
	};

	const openEditMember = (member: TeamMember) => {
		setEditingMember(member);
		setMemberForm({ user_id: member.user_id, role: member.role });
	};

	const openEditProject = (project: Project) => {
		setEditingProject(project);
		setProjectForm({
			name: project.name,
			description: project.description ?? "",
			metadata: project.metadata
				? JSON.stringify(project.metadata, null, 2)
				: "",
		});
	};

	const isLoading = isLoadingTeam || isLoadingMembers || isLoadingProjects;
	const mutationError =
		updateTeam.error ||
		deleteTeam.error ||
		addMember.error ||
		updateMemberRole.error ||
		removeMember.error ||
		createProject.error ||
		updateProject.error ||
		deleteProject.error;

	if (isLoading && !team) {
		return (
			<Box className="flex-1 items-center justify-center bg-background-0">
				<Spinner size="large" />
			</Box>
		);
	}

	if (!team) {
		return (
			<Box className="flex-1 items-center justify-center bg-background-0 p-6">
				<EmptyState
					icon={<Users size={32} className="text-typography-400" />}
					title="Team not found"
					description="The team you are looking for does not exist or you do not have access."
				/>
			</Box>
		);
	}

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-5xl">
				<Link href="/(tabs)/(admin)/teams" asChild>
					<Button
						size="xs"
						variant="link"
						action="secondary"
						className="self-start -ml-2"
					>
						<ButtonIcon as={ChevronLeft} />
						<ButtonText>Back to teams</ButtonText>
					</Button>
				</Link>

				<TeamHeader
					team={team}
					onEdit={() => {
						resetTeamForm(team);
						setShowEditTeam(true);
					}}
					onDelete={() => setDeleteTeamTarget(team)}
				/>

				{mutationError && (
					<Alert action="error">
						<AlertText>
							{mutationError instanceof Error
								? mutationError.message
								: "An error occurred"}
						</AlertText>
					</Alert>
				)}

				{/* Projects Section */}
				<VStack className="gap-4">
					<HStack className="justify-between items-center">
						<HStack className="items-center gap-2">
							<FolderKanban size={22} className="text-primary-500" />
							<Heading size="lg">Projects</Heading>
						</HStack>
						<Button
							size="sm"
							onPress={() => {
								resetProjectForm();
								setShowCreateProject(true);
							}}
						>
							<ButtonIcon as={Plus} />
							<ButtonText>Add Project</ButtonText>
						</Button>
					</HStack>

					{!projects || projects.length === 0 ? (
						<Card className="p-6">
							<EmptyState
								icon={
									<FolderKanban size={28} className="text-typography-400" />
								}
								title="No projects yet"
								description="Add a project to organize work within this team"
							/>
						</Card>
					) : (
						<VStack className="gap-2">
							{projects.map((project) => (
								<ProjectRow
									key={project.id}
									project={project}
									onEdit={() => openEditProject(project)}
									onDelete={() => setDeleteProjectTarget(project)}
								/>
							))}
						</VStack>
					)}
				</VStack>

				<Divider />

				{/* Members Section */}
				<VStack className="gap-4">
					<HStack className="justify-between items-center">
						<HStack className="items-center gap-2">
							<Shield size={22} className="text-primary-500" />
							<Heading size="lg">Members</Heading>
						</HStack>
						<Button
							size="sm"
							onPress={() => {
								resetMemberForm();
								setShowAddMember(true);
							}}
						>
							<ButtonIcon as={Plus} />
							<ButtonText>Add Member</ButtonText>
						</Button>
					</HStack>

					{!members || members.length === 0 ? (
						<Card className="p-6">
							<EmptyState
								icon={<Users size={28} className="text-typography-400" />}
								title="No members yet"
								description="Add members to collaborate in this team"
							/>
						</Card>
					) : (
						<VStack className="gap-2">
							{members.map((member) => (
								<MemberRow
									key={member.id}
									member={member}
									onEdit={() => openEditMember(member)}
									onRemove={() => setRemoveMemberTarget(member)}
								/>
							))}
						</VStack>
					)}
				</VStack>
			</Box>

			{/* Edit Team Modal */}
			<Modal
				isOpen={showEditTeam}
				onClose={() => setShowEditTeam(false)}
				size="md"
			>
				<ModalBackdrop />
				<ModalContent>
					<ModalHeader>
						<Heading size="md">Edit Team</Heading>
					</ModalHeader>
					<ModalBody>
						<VStack className="gap-4">
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Name</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={teamForm.name}
										onChangeText={(v) =>
											setTeamForm((p) => ({ ...p, name: v }))
										}
									/>
								</Input>
							</FormControl>
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Description</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={teamForm.description}
										onChangeText={(v) =>
											setTeamForm((p) => ({ ...p, description: v }))
										}
									/>
								</Input>
							</FormControl>
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Active</FormControlLabelText>
								</FormControlLabel>
								<HStack className="gap-2">
									<Button
										size="sm"
										variant={teamForm.is_active ? "solid" : "outline"}
										action={teamForm.is_active ? "primary" : "secondary"}
										onPress={() =>
											setTeamForm((p) => ({ ...p, is_active: true }))
										}
									>
										<ButtonText>Active</ButtonText>
									</Button>
									<Button
										size="sm"
										variant={!teamForm.is_active ? "solid" : "outline"}
										action={!teamForm.is_active ? "primary" : "secondary"}
										onPress={() =>
											setTeamForm((p) => ({ ...p, is_active: false }))
										}
									>
										<ButtonText>Inactive</ButtonText>
									</Button>
								</HStack>
							</FormControl>
						</VStack>
					</ModalBody>
					<ModalFooter className="gap-3">
						<Button
							variant="outline"
							action="secondary"
							onPress={() => setShowEditTeam(false)}
						>
							<ButtonText>Cancel</ButtonText>
						</Button>
						<Button
							onPress={handleUpdateTeam}
							isDisabled={!teamForm.name.trim() || updateTeam.isPending}
						>
							{updateTeam.isPending && <ButtonSpinner />}
							<ButtonText>Save</ButtonText>
						</Button>
					</ModalFooter>
				</ModalContent>
			</Modal>

			{/* Add Member Modal */}
			<Modal
				isOpen={showAddMember}
				onClose={() => setShowAddMember(false)}
				size="md"
			>
				<ModalBackdrop />
				<ModalContent>
					<ModalHeader>
						<Heading size="md">Add Member</Heading>
					</ModalHeader>
					<ModalBody>
						<VStack className="gap-4">
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>User ID</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={memberForm.user_id}
										onChangeText={(v) =>
											setMemberForm((p) => ({ ...p, user_id: v }))
										}
										placeholder="e.g. 550e8400-e29b-41d4-a716-446655440000"
									/>
								</Input>
							</FormControl>
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Role</FormControlLabelText>
								</FormControlLabel>
								<Select
									selectedValue={memberForm.role}
									onValueChange={(v) =>
										setMemberForm((p) => ({ ...p, role: v }))
									}
								>
									<SelectTrigger variant="outline" size="md">
										<SelectInput placeholder="Select role" />
										<SelectIcon as={ChevronDown} className="mr-3" />
									</SelectTrigger>
									<SelectPortal>
										<SelectBackdrop />
										<SelectContent>
											<SelectDragIndicatorWrapper>
												<SelectDragIndicator />
											</SelectDragIndicatorWrapper>
											{TEAM_ROLES.map((r) => (
												<SelectItem
													key={r.value}
													label={r.label}
													value={r.value}
												/>
											))}
										</SelectContent>
									</SelectPortal>
								</Select>
							</FormControl>
						</VStack>
					</ModalBody>
					<ModalFooter className="gap-3">
						<Button
							variant="outline"
							action="secondary"
							onPress={() => setShowAddMember(false)}
						>
							<ButtonText>Cancel</ButtonText>
						</Button>
						<Button
							onPress={handleAddMember}
							isDisabled={
								!memberForm.user_id.trim() ||
								!memberForm.role ||
								addMember.isPending
							}
						>
							{addMember.isPending && <ButtonSpinner />}
							<ButtonText>Add</ButtonText>
						</Button>
					</ModalFooter>
				</ModalContent>
			</Modal>

			{/* Edit Member Role Modal */}
			<Modal
				isOpen={!!editingMember}
				onClose={() => setEditingMember(null)}
				size="md"
			>
				<ModalBackdrop />
				<ModalContent>
					<ModalHeader>
						<Heading size="md">Update Member Role</Heading>
					</ModalHeader>
					<ModalBody>
						<VStack className="gap-4">
							<Text size="sm" className="text-typography-500">
								Update role for{" "}
								<Text className="font-semibold">{editingMember?.username}</Text>
							</Text>
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Role</FormControlLabelText>
								</FormControlLabel>
								<Select
									selectedValue={memberForm.role}
									onValueChange={(v) =>
										setMemberForm((p) => ({ ...p, role: v }))
									}
								>
									<SelectTrigger variant="outline" size="md">
										<SelectInput placeholder="Select role" />
										<SelectIcon as={ChevronDown} className="mr-3" />
									</SelectTrigger>
									<SelectPortal>
										<SelectBackdrop />
										<SelectContent>
											<SelectDragIndicatorWrapper>
												<SelectDragIndicator />
											</SelectDragIndicatorWrapper>
											{TEAM_ROLES.map((r) => (
												<SelectItem
													key={r.value}
													label={r.label}
													value={r.value}
												/>
											))}
										</SelectContent>
									</SelectPortal>
								</Select>
							</FormControl>
						</VStack>
					</ModalBody>
					<ModalFooter className="gap-3">
						<Button
							variant="outline"
							action="secondary"
							onPress={() => setEditingMember(null)}
						>
							<ButtonText>Cancel</ButtonText>
						</Button>
						<Button
							onPress={handleUpdateMemberRole}
							isDisabled={!memberForm.role || updateMemberRole.isPending}
						>
							{updateMemberRole.isPending && <ButtonSpinner />}
							<ButtonText>Save</ButtonText>
						</Button>
					</ModalFooter>
				</ModalContent>
			</Modal>

			{/* Create Project Modal */}
			<Modal
				isOpen={showCreateProject}
				onClose={() => setShowCreateProject(false)}
				size="md"
			>
				<ModalBackdrop />
				<ModalContent>
					<ModalHeader>
						<Heading size="md">Add Project</Heading>
					</ModalHeader>
					<ModalBody>
						<VStack className="gap-4">
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Name</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={projectForm.name}
										onChangeText={(v) =>
											setProjectForm((p) => ({ ...p, name: v }))
										}
										placeholder="e.g. API Gateway"
									/>
								</Input>
							</FormControl>
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Description</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={projectForm.description}
										onChangeText={(v) =>
											setProjectForm((p) => ({ ...p, description: v }))
										}
										placeholder="Optional description"
									/>
								</Input>
							</FormControl>
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Metadata (JSON)</FormControlLabelText>
								</FormControlLabel>
								<FormControlHelper>
									<FormControlHelperText>
										Optional JSON metadata for this project.
									</FormControlHelperText>
								</FormControlHelper>
								<Textarea>
									<TextareaInput
										value={projectForm.metadata}
										onChangeText={(v) =>
											setProjectForm((p) => ({ ...p, metadata: v }))
										}
										placeholder='{"environment": "production"}'
									/>
								</Textarea>
							</FormControl>
						</VStack>
					</ModalBody>
					<ModalFooter className="gap-3">
						<Button
							variant="outline"
							action="secondary"
							onPress={() => setShowCreateProject(false)}
						>
							<ButtonText>Cancel</ButtonText>
						</Button>
						<Button
							onPress={handleCreateProject}
							isDisabled={!projectForm.name.trim() || createProject.isPending}
						>
							{createProject.isPending && <ButtonSpinner />}
							<ButtonText>Create</ButtonText>
						</Button>
					</ModalFooter>
				</ModalContent>
			</Modal>

			{/* Edit Project Modal */}
			<Modal
				isOpen={!!editingProject}
				onClose={() => setEditingProject(null)}
				size="md"
			>
				<ModalBackdrop />
				<ModalContent>
					<ModalHeader>
						<Heading size="md">Edit Project</Heading>
					</ModalHeader>
					<ModalBody>
						<VStack className="gap-4">
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Name</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={projectForm.name}
										onChangeText={(v) =>
											setProjectForm((p) => ({ ...p, name: v }))
										}
									/>
								</Input>
							</FormControl>
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Description</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={projectForm.description}
										onChangeText={(v) =>
											setProjectForm((p) => ({ ...p, description: v }))
										}
									/>
								</Input>
							</FormControl>
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Metadata (JSON)</FormControlLabelText>
								</FormControlLabel>
								<FormControlHelper>
									<FormControlHelperText>
										Optional JSON metadata for this project.
									</FormControlHelperText>
								</FormControlHelper>
								<Textarea>
									<TextareaInput
										value={projectForm.metadata}
										onChangeText={(v) =>
											setProjectForm((p) => ({ ...p, metadata: v }))
										}
									/>
								</Textarea>
							</FormControl>
						</VStack>
					</ModalBody>
					<ModalFooter className="gap-3">
						<Button
							variant="outline"
							action="secondary"
							onPress={() => setEditingProject(null)}
						>
							<ButtonText>Cancel</ButtonText>
						</Button>
						<Button
							onPress={handleUpdateProject}
							isDisabled={!projectForm.name.trim() || updateProject.isPending}
						>
							{updateProject.isPending && <ButtonSpinner />}
							<ButtonText>Save</ButtonText>
						</Button>
					</ModalFooter>
				</ModalContent>
			</Modal>

			{/* Confirm Dialogs */}
			<ConfirmDialog
				open={!!deleteTeamTarget}
				onOpenChange={(open) => !open && setDeleteTeamTarget(null)}
				title="Delete Team"
				description={
					deleteTeamTarget
						? `Delete "${deleteTeamTarget.name}"? This will soft-delete all projects and memberships.`
						: ""
				}
				confirmText="Delete"
				onConfirm={handleDeleteTeam}
				destructive
			/>

			<ConfirmDialog
				open={!!deleteProjectTarget}
				onOpenChange={(open) => !open && setDeleteProjectTarget(null)}
				title="Delete Project"
				description={
					deleteProjectTarget
						? `Delete project "${deleteProjectTarget.name}"?`
						: ""
				}
				confirmText="Delete"
				onConfirm={handleDeleteProject}
				destructive
			/>

			<ConfirmDialog
				open={!!removeMemberTarget}
				onOpenChange={(open) => !open && setRemoveMemberTarget(null)}
				title="Remove Member"
				description={
					removeMemberTarget
						? `Remove ${removeMemberTarget.username} from the team?`
						: ""
				}
				confirmText="Remove"
				onConfirm={handleRemoveMember}
				destructive
			/>
		</ScrollView>
	);
}
