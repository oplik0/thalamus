"use client";

import { Link } from "expo-router";
import {
	Activity,
	CheckCircle2,
	KeyRound,
	Loader2,
	Plus,
	Settings,
	Shield,
	User,
	XCircle,
} from "lucide-react-native";
import { ScrollView, View } from "react-native";
import { PageHeader } from "@/components/page-header";
import { Badge, BadgeText } from "@/components/ui/badge";
import { Box } from "@/components/ui/box";
import { Button, ButtonIcon, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Heading } from "@/components/ui/heading";
import { HStack } from "@/components/ui/hstack";
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { VStack } from "@/components/ui/vstack";
import { useAuth } from "@/contexts/auth-context";
import { useApiKeys } from "@/hooks/use-api-keys";
import { useHealthCheck } from "@/hooks/use-health-check";
import { useSigningKeys } from "@/hooks/use-signing-keys";

function HealthStatusCard() {
	const { data, isLoading, isError } = useHealthCheck();

	return (
		<Card className="flex-1 min-w-[220px] p-5 gap-3">
			<HStack className="items-center gap-2">
				{isLoading ? (
					<Loader2 size={22} className="text-warning-500" />
				) : isError ? (
					<XCircle size={22} className="text-error-500" />
				) : (
					<CheckCircle2 size={22} className="text-success-500" />
				)}
				<Heading size="sm">Backend Health</Heading>
			</HStack>
			<Text size="sm" className="text-typography-500">
				{isLoading
					? "Checking..."
					: isError
						? "Backend unreachable"
						: "All systems operational"}
			</Text>
			{data?.version && (
				<Badge action="info" variant="outline" size="sm" className="self-start">
					<BadgeText>v{data.version}</BadgeText>
				</Badge>
			)}
		</Card>
	);
}

function UserInfoCard() {
	const { user, isLoading } = useAuth();

	return (
		<Card className="flex-1 min-w-[220px] p-5 gap-3">
			<HStack className="items-center gap-2">
				<User size={22} className="text-primary-500" />
				<Heading size="sm">Current User</Heading>
			</HStack>
			{isLoading ? (
				<Spinner size="small" />
			) : user ? (
				<VStack className="gap-1.5">
					<InfoRow label="User ID" value={user.user_id} />
					<InfoRow label="Team" value={user.team_id} />
					<InfoRow
						label="Roles"
						value={
							user.roles && user.roles.length > 0
								? user.roles.join(", ")
								: "None"
						}
					/>
				</VStack>
			) : (
				<Text size="sm" className="text-typography-500">
					Not logged in
				</Text>
			)}
		</Card>
	);
}

function InfoRow({ label, value }: { label: string; value: string }) {
	return (
		<HStack className="justify-between items-center">
			<Text size="xs" className="text-typography-500">
				{label}
			</Text>
			<Text size="xs" className="font-mono" selectable>
				{value.length > 16 ? `${value.slice(0, 16)}...` : value}
			</Text>
		</HStack>
	);
}

function QuickStats() {
	const { data: apiKeys } = useApiKeys();
	const { data: signingKeys } = useSigningKeys();

	const activeApiKeys = apiKeys?.filter((k) => k.is_active).length ?? 0;
	const activeSigningKeys = signingKeys?.filter((k) => k.is_active).length ?? 0;

	return (
		<Card className="flex-1 min-w-[220px] p-5 gap-3">
			<HStack className="items-center gap-2">
				<Activity size={22} className="text-primary-500" />
				<Heading size="sm">Quick Stats</Heading>
			</HStack>
			<HStack className="gap-6">
				<VStack className="gap-0.5">
					<Heading size="2xl">{activeApiKeys}</Heading>
					<Text size="xs" className="text-typography-500">
						API Keys
					</Text>
				</VStack>
				<VStack className="gap-0.5">
					<Heading size="2xl">{activeSigningKeys}</Heading>
					<Text size="xs" className="text-typography-500">
						Signing Keys
					</Text>
				</VStack>
			</HStack>
		</Card>
	);
}

function QuickActions() {
	const actions = [
		{
			name: "Create Team",
			href: "/(tabs)/(admin)/teams/create",
			icon: Plus,
		},
		{
			name: "Create API Key",
			href: "/(tabs)/(admin)/api-keys/create",
			icon: Plus,
		},
		{
			name: "Create Signing Key",
			href: "/(tabs)/(admin)/signing-keys/create",
			icon: Plus,
		},
		{
			name: "Manage Policies",
			href: "/(tabs)/(admin)/authorization",
			icon: Shield,
		},
		{ name: "Settings", href: "/(tabs)/(admin)/settings", icon: Settings },
	] as const;

	return (
		<Card className="flex-1 min-w-[220px] p-5 gap-3">
			<HStack className="items-center gap-2">
				<KeyRound size={22} className="text-primary-500" />
				<Heading size="sm">Quick Actions</Heading>
			</HStack>
			<View className="flex-row flex-wrap gap-2">
				{actions.map((action) => (
					<Link key={action.name} href={action.href} asChild>
						<Button size="sm" variant="outline" action="secondary">
							<ButtonIcon as={action.icon} />
							<ButtonText>{action.name}</ButtonText>
						</Button>
					</Link>
				))}
			</View>
		</Card>
	);
}

export default function AdminDashboard() {
	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-5xl">
				<PageHeader
					title="Dashboard"
					description="Thalamus Admin Panel overview"
				/>

				<View className="flex-row flex-wrap gap-4">
					<HealthStatusCard />
					<UserInfoCard />
				</View>

				<View className="flex-row flex-wrap gap-4">
					<QuickStats />
					<QuickActions />
				</View>
			</Box>
		</ScrollView>
	);
}
