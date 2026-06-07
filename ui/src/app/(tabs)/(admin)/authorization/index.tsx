"use client";

import { Plus, Search, Shield, Trash2, Users } from "lucide-react-native";
import { useState } from "react";
import { ScrollView, View } from "react-native";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
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
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { VStack } from "@/components/ui/vstack";
import {
	useAssignRole,
	useCreatePolicy,
	useDeletePolicy,
	usePolicies,
	useRemoveRole,
	useRolesByUserDomain,
} from "@/hooks/use-authorization";
import type { CreatePolicyRequest, PolicyInfo } from "@/lib/types";

// ─── Policies Section ──────────────────────────────────────

function PoliciesSection() {
	const { data: policies, isLoading } = usePolicies();
	const createPolicy = useCreatePolicy();
	const deletePolicy = useDeletePolicy();
	const toast = useToast();

	const [showCreate, setShowCreate] = useState(false);
	const [deleteTarget, setDeleteTarget] = useState<PolicyInfo | null>(null);
	const [newPolicy, setNewPolicy] = useState<CreatePolicyRequest>({
		subject: "",
		domain: "",
		object: "",
		action: "",
	});

	const handleCreate = async () => {
		await createPolicy.mutateAsync(newPolicy);
		setShowCreate(false);
		setNewPolicy({ subject: "", domain: "", object: "", action: "" });
		toast.show({
			id: "policy-created",
			render: () => (
				<Toast action="success">
					<ToastTitle>Policy created</ToastTitle>
				</Toast>
			),
		});
	};

	const handleDelete = async () => {
		if (!deleteTarget) return;
		await deletePolicy.mutateAsync({
			subject: deleteTarget.subject,
			domain: deleteTarget.domain,
			object: deleteTarget.object,
			action: deleteTarget.action,
		});
		toast.show({
			id: "policy-deleted",
			render: () => (
				<Toast action="success">
					<ToastTitle>Policy deleted</ToastTitle>
				</Toast>
			),
		});
	};

	return (
		<VStack className="gap-4">
			<HStack className="justify-between items-center">
				<HStack className="items-center gap-2">
					<Shield size={22} className="text-primary-500" />
					<Heading size="lg">Policies</Heading>
				</HStack>
				<Button size="sm" onPress={() => setShowCreate(true)}>
					<ButtonIcon as={Plus} />
					<ButtonText>Add Policy</ButtonText>
				</Button>
			</HStack>

			{isLoading ? (
				<Box className="py-8 items-center">
					<Spinner size="large" />
				</Box>
			) : !policies || policies.length === 0 ? (
				<Card className="p-6">
					<EmptyState
						icon={<Shield size={28} className="text-typography-400" />}
						title="No policies configured"
						description="Add Casbin policies to control access"
					/>
				</Card>
			) : (
				<VStack className="gap-2">
					{policies.map((policy, idx) => {
						const key = `${policy.subject}-${policy.domain}-${policy.object}-${policy.action}-${idx}`;
						return (
							<Card key={key} className="p-3">
								<HStack className="justify-between items-center">
									<HStack className="gap-2 flex-wrap flex-1">
										<Badge action="info" size="sm">
											<BadgeText>{policy.subject}</BadgeText>
										</Badge>
										<Badge action="muted" size="sm">
											<BadgeText>{policy.domain}</BadgeText>
										</Badge>
										<Badge action="muted" size="sm">
											<BadgeText>{policy.object}</BadgeText>
										</Badge>
										<Badge
											action={policy.action === "write" ? "warning" : "success"}
											size="sm"
										>
											<BadgeText>{policy.action}</BadgeText>
										</Badge>
									</HStack>
									<Button
										size="xs"
										variant="outline"
										action="negative"
										onPress={() => setDeleteTarget(policy)}
										accessibilityLabel="Delete policy"
									>
										<ButtonIcon as={Trash2} />
									</Button>
								</HStack>
							</Card>
						);
					})}
				</VStack>
			)}

			{/* Create Policy Modal */}
			<Modal isOpen={showCreate} onClose={() => setShowCreate(false)} size="md">
				<ModalBackdrop />
				<ModalContent>
					<ModalHeader>
						<Heading size="md">Add Policy</Heading>
					</ModalHeader>
					<ModalBody>
						<VStack className="gap-4">
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Subject</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={newPolicy.subject}
										onChangeText={(v) =>
											setNewPolicy((p) => ({ ...p, subject: v }))
										}
										placeholder="e.g. alice, role:admin"
									/>
								</Input>
							</FormControl>
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Domain</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={newPolicy.domain}
										onChangeText={(v) =>
											setNewPolicy((p) => ({ ...p, domain: v }))
										}
										placeholder="e.g. team-1"
									/>
								</Input>
							</FormControl>
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Object</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={newPolicy.object}
										onChangeText={(v) =>
											setNewPolicy((p) => ({ ...p, object: v }))
										}
										placeholder="e.g. api-keys, models"
									/>
								</Input>
							</FormControl>
							<FormControl isRequired>
								<FormControlLabel>
									<FormControlLabelText>Action</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputField
										value={newPolicy.action}
										onChangeText={(v) =>
											setNewPolicy((p) => ({ ...p, action: v }))
										}
										placeholder="e.g. read, write, delete"
									/>
								</Input>
							</FormControl>
						</VStack>
					</ModalBody>
					<ModalFooter className="gap-3">
						<Button
							variant="outline"
							action="secondary"
							onPress={() => setShowCreate(false)}
						>
							<ButtonText>Cancel</ButtonText>
						</Button>
						<Button
							onPress={handleCreate}
							isDisabled={
								!newPolicy.subject ||
								!newPolicy.domain ||
								!newPolicy.object ||
								!newPolicy.action ||
								createPolicy.isPending
							}
						>
							{createPolicy.isPending && <ButtonSpinner />}
							<ButtonText>Create</ButtonText>
						</Button>
					</ModalFooter>
				</ModalContent>
			</Modal>

			<ConfirmDialog
				open={!!deleteTarget}
				onOpenChange={(open) => !open && setDeleteTarget(null)}
				title="Delete Policy"
				description={
					deleteTarget
						? `Delete policy: ${deleteTarget.subject} can ${deleteTarget.action} ${deleteTarget.object} in ${deleteTarget.domain}?`
						: ""
				}
				confirmText="Delete"
				onConfirm={handleDelete}
				destructive
			/>
		</VStack>
	);
}

// ─── Roles Section ─────────────────────────────────────────

function RolesSection() {
	const [lookupUser, setLookupUser] = useState("");
	const [lookupDomain, setLookupDomain] = useState("");
	const [searchActive, setSearchActive] = useState(false);
	const toast = useToast();

	const { data: rolesResponse, isLoading: isLoadingRoles } =
		useRolesByUserDomain(
			searchActive ? lookupUser : "",
			searchActive ? lookupDomain : "",
		);

	const assignRole = useAssignRole();
	const removeRoleMutation = useRemoveRole();

	const [newRole, setNewRole] = useState({
		user: "",
		role: "",
		domain: "",
	});

	const handleSearch = () => {
		if (lookupUser && lookupDomain) {
			setSearchActive(true);
		}
	};

	const handleAssign = async () => {
		await assignRole.mutateAsync(newRole);
		setNewRole({ user: "", role: "", domain: "" });
		toast.show({
			id: "role-assigned",
			render: () => (
				<Toast action="success">
					<ToastTitle>Role assigned</ToastTitle>
				</Toast>
			),
		});
	};

	const handleRemove = async (role: string) => {
		if (!rolesResponse) return;
		await removeRoleMutation.mutateAsync({
			user: rolesResponse.user,
			domain: rolesResponse.domain,
			role,
		});
		toast.show({
			id: `role-removed-${role}`,
			render: () => (
				<Toast action="success">
					<ToastTitle>Role removed</ToastTitle>
				</Toast>
			),
		});
	};

	return (
		<VStack className="gap-4">
			<HStack className="items-center gap-2">
				<Users size={22} className="text-primary-500" />
				<Heading size="lg">Roles</Heading>
			</HStack>

			{/* Look up roles by user + domain */}
			<Card className="p-4 gap-4">
				<Heading size="sm">Look up user roles</Heading>
				<HStack className="gap-3 flex-wrap">
					<FormControl className="flex-1 min-w-[140px]">
						<Input>
							<InputField
								value={lookupUser}
								onChangeText={(v) => {
									setLookupUser(v);
									setSearchActive(false);
								}}
								placeholder="User ID"
							/>
						</Input>
					</FormControl>
					<FormControl className="flex-1 min-w-[140px]">
						<Input>
							<InputField
								value={lookupDomain}
								onChangeText={(v) => {
									setLookupDomain(v);
									setSearchActive(false);
								}}
								placeholder="Domain"
							/>
						</Input>
					</FormControl>
					<Button
						size="md"
						onPress={handleSearch}
						isDisabled={!lookupUser || !lookupDomain}
					>
						<ButtonIcon as={Search} />
						<ButtonText>Search</ButtonText>
					</Button>
				</HStack>

				{searchActive && isLoadingRoles && (
					<Box className="py-4 items-center">
						<Spinner />
					</Box>
				)}

				{searchActive && rolesResponse && (
					<VStack className="gap-2 mt-2">
						{rolesResponse.roles.length === 0 ? (
							<Text size="sm" className="text-typography-500">
								No roles found for this user/domain
							</Text>
						) : (
							rolesResponse.roles.map((role) => (
								<HStack
									key={role}
									className="justify-between items-center p-2 bg-background-50 rounded-lg"
								>
									<Badge action="info" size="md">
										<BadgeText>{role}</BadgeText>
									</Badge>
									<Button
										size="xs"
										variant="outline"
										action="negative"
										onPress={() => handleRemove(role)}
									>
										<ButtonIcon as={Trash2} />
									</Button>
								</HStack>
							))
						)}
					</VStack>
				)}
			</Card>

			{/* Assign role */}
			<Card className="p-4 gap-4">
				<Heading size="sm">Assign role</Heading>
				<HStack className="gap-3 flex-wrap">
					<FormControl className="flex-1 min-w-[120px]">
						<Input>
							<InputField
								value={newRole.user}
								onChangeText={(v) => setNewRole((p) => ({ ...p, user: v }))}
								placeholder="User ID"
							/>
						</Input>
					</FormControl>
					<FormControl className="flex-1 min-w-[120px]">
						<Input>
							<InputField
								value={newRole.role}
								onChangeText={(v) => setNewRole((p) => ({ ...p, role: v }))}
								placeholder="Role name"
							/>
						</Input>
					</FormControl>
					<FormControl className="flex-1 min-w-[120px]">
						<Input>
							<InputField
								value={newRole.domain}
								onChangeText={(v) => setNewRole((p) => ({ ...p, domain: v }))}
								placeholder="Domain"
							/>
						</Input>
					</FormControl>
					<Button
						size="md"
						onPress={handleAssign}
						isDisabled={
							!newRole.user ||
							!newRole.role ||
							!newRole.domain ||
							assignRole.isPending
						}
					>
						{assignRole.isPending && <ButtonSpinner />}
						<ButtonText>Assign</ButtonText>
					</Button>
				</HStack>
			</Card>
		</VStack>
	);
}

// ─── Main Page ─────────────────────────────────────────────

export default function AuthorizationPage() {
	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-8 max-w-5xl">
				<PageHeader
					title="Authorization"
					description="Manage Casbin policies and role assignments for access control"
				/>

				<PoliciesSection />

				<Divider />

				<RolesSection />
			</Box>
		</ScrollView>
	);
}
