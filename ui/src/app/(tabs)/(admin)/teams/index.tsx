"use client";

import { Link } from "expo-router";
import {
	ArrowRight,
	FolderKanban,
	Plus,
	Trash2,
	Users,
} from "lucide-react-native";
import { useState } from "react";
import { ScrollView } from "react-native";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { Badge, BadgeText } from "@/components/ui/badge";
import { Box } from "@/components/ui/box";
import { Button, ButtonIcon, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { HStack } from "@/components/ui/hstack";
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { VStack } from "@/components/ui/vstack";
import { useDeleteTeam, useTeams } from "@/hooks/use-teams";
import type { Team } from "@/lib/types";

function TeamCard({ team, onDelete }: { team: Team; onDelete: () => void }) {
	return (
		<Link href={`/(tabs)/(admin)/teams/${team.id}`} asChild>
			<Card className="p-4 active:opacity-70 cursor-pointer">
				<HStack className="justify-between items-start">
					<VStack className="flex-1 gap-1.5">
						<HStack className="items-center gap-2">
							<Users size={18} className="text-primary-500" />
							<Text className="font-semibold text-base">{team.name}</Text>
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

						<HStack className="items-center gap-4 mt-1 flex-wrap">
							{team.parent_team_id && (
								<Badge action="info" variant="outline" size="sm">
									<BadgeText>Has parent team</BadgeText>
								</Badge>
							)}
							<HStack className="items-center gap-1">
								<FolderKanban size={12} className="text-typography-400" />
								<Text size="xs" className="text-typography-400">
									Projects & members
								</Text>
							</HStack>
						</HStack>
					</VStack>

					<HStack className="items-center gap-2">
						<Button
							size="xs"
							variant="outline"
							action="negative"
							onPress={(e) => {
								e.stopPropagation();
								onDelete();
							}}
							accessibilityLabel="Delete team"
						>
							<ButtonIcon as={Trash2} />
						</Button>
						<ArrowRight size={18} className="text-typography-400" />
					</HStack>
				</HStack>
			</Card>
		</Link>
	);
}

export default function TeamsPage() {
	const { data: teams, isLoading } = useTeams();
	const deleteMutation = useDeleteTeam();
	const toast = useToast();

	const [deleteTarget, setDeleteTarget] = useState<Team | null>(null);

	const handleDelete = async () => {
		if (!deleteTarget) return;
		await deleteMutation.mutateAsync(deleteTarget.id);
		toast.show({
			id: `delete-team-${deleteTarget.id}`,
			render: () => (
				<Toast action="success">
					<ToastTitle>Team deleted</ToastTitle>
				</Toast>
			),
		});
	};

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-5xl">
				<PageHeader
					title="Teams"
					description="Manage teams, their members, and projects"
					actions={
						<Link href="/(tabs)/(admin)/teams/create" asChild>
							<Button size="sm">
								<ButtonIcon as={Plus} />
								<ButtonText>Create Team</ButtonText>
							</Button>
						</Link>
					}
				/>

				{isLoading ? (
					<Box className="py-12 items-center">
						<Spinner size="large" />
					</Box>
				) : !teams || teams.length === 0 ? (
					<EmptyState
						icon={<Users size={32} className="text-typography-400" />}
						title="No teams yet"
						description="Create your first team to organize members and projects"
					/>
				) : (
					<VStack className="gap-3">
						{teams.map((team) => (
							<TeamCard
								key={team.id}
								team={team}
								onDelete={() => setDeleteTarget(team)}
							/>
						))}
					</VStack>
				)}
			</Box>

			<ConfirmDialog
				open={!!deleteTarget}
				onOpenChange={(open) => !open && setDeleteTarget(null)}
				title="Delete Team"
				description={
					deleteTarget
						? `Are you sure you want to delete "${deleteTarget.name}"? This will also soft-delete all projects and memberships in this team.`
						: ""
				}
				confirmText="Delete"
				onConfirm={handleDelete}
				destructive
			/>
		</ScrollView>
	);
}
