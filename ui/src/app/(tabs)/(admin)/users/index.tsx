"use client";

import { Link } from "expo-router";
import { Plus, UserPlus, Users } from "lucide-react-native";
import { ScrollView } from "react-native";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { Badge, BadgeText } from "@/components/ui/badge";
import { Box } from "@/components/ui/box";
import { Button, ButtonIcon, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { HStack } from "@/components/ui/hstack";
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { VStack } from "@/components/ui/vstack";
import { useUsers } from "@/hooks/use-users";
import type { UserInfo } from "@/lib/types";

function UserCard({ user }: { user: UserInfo }) {
	return (
		<Card className="p-4 gap-2">
			<HStack className="items-start justify-between gap-3">
				<VStack className="gap-1 flex-1">
					<HStack className="items-center gap-2 flex-wrap">
						<Users size={18} className="text-primary-500" />
						<Text className="font-semibold text-base">{user.username}</Text>
						<Badge action={user.is_active ? "success" : "muted"} size="sm">
							<BadgeText>{user.is_active ? "Active" : "Inactive"}</BadgeText>
						</Badge>
						{user.has_password && (
							<Badge action="info" variant="outline" size="sm">
								<BadgeText>Password</BadgeText>
							</Badge>
						)}
					</HStack>
					<Text size="sm" className="text-typography-500">
						{user.email}
					</Text>
					<Text size="xs" className="text-typography-400 font-mono" selectable>
						{user.id}
					</Text>
				</VStack>
				<Text size="xs" className="text-typography-400">
					{new Date(user.created_at).toLocaleDateString()}
				</Text>
			</HStack>
		</Card>
	);
}

export default function UsersPage() {
	const { data: users, isLoading } = useUsers();

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-5xl">
				<PageHeader
					title="Users"
					description="Create and review users that can sign in to Thalamus"
					actions={
						<Link href="/(tabs)/(admin)/users/create" asChild>
							<Button size="sm">
								<ButtonIcon as={Plus} />
								<ButtonText>Create User</ButtonText>
							</Button>
						</Link>
					}
				/>

				{isLoading ? (
					<Box className="py-12 items-center">
						<Spinner size="large" />
					</Box>
				) : !users || users.length === 0 ? (
					<EmptyState
						icon={<UserPlus size={32} className="text-typography-400" />}
						title="No users yet"
						description="Create a user with an initial password to enable sign in"
					/>
				) : (
					<VStack className="gap-3">
						{users.map((user) => (
							<UserCard key={user.id} user={user} />
						))}
					</VStack>
				)}
			</Box>
		</ScrollView>
	);
}
