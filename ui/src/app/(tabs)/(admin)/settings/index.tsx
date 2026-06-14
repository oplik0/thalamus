"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useRouter } from "expo-router";
import {
	Clock,
	KeyRound,
	LogOut,
	ShieldCheck,
	Trash2,
	User,
} from "lucide-react-native";
import { useState } from "react";
import { ScrollView, View } from "react-native";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { PageHeader } from "@/components/page-header";
import { Badge, BadgeText } from "@/components/ui/badge";
import { Box } from "@/components/ui/box";
import { Button, ButtonIcon, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Divider } from "@/components/ui/divider";
import { Heading } from "@/components/ui/heading";
import { HStack } from "@/components/ui/hstack";
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { VStack } from "@/components/ui/vstack";
import { useAuth } from "@/contexts/auth-context";
import type { RefreshTokenInfo } from "@/lib/types";
import { listRefreshTokens, revokeRefreshToken } from "@/services/auth";

function UserInfoSection() {
	const { user } = useAuth();

	return (
		<Card className="p-5 gap-4">
			<HStack className="items-center gap-2">
				<User size={22} className="text-primary-500" />
				<Heading size="md">User Information</Heading>
			</HStack>

			{user ? (
				<VStack className="gap-2.5">
					<InfoRow label="User ID" value={user.user_id} selectable />
					<InfoRow label="Team ID" value={user.team_id} selectable />
					<Divider />
					<View className="gap-1.5">
						<Text size="xs" className="text-typography-500 font-semibold">
							Roles
						</Text>
						{user.roles && user.roles.length > 0 ? (
							<HStack className="gap-2 flex-wrap">
								{user.roles.map((role) => (
									<Badge key={role} action="info" size="sm">
										<BadgeText>{role}</BadgeText>
									</Badge>
								))}
							</HStack>
						) : (
							<Text size="sm" className="text-typography-500">
								No roles assigned
							</Text>
						)}
					</View>
					<View className="gap-1.5">
						<Text size="xs" className="text-typography-500 font-semibold">
							Scopes
						</Text>
						{user.scopes && user.scopes.length > 0 ? (
							<HStack className="gap-2 flex-wrap">
								{user.scopes.map((scope) => (
									<Badge key={scope} action="muted" size="sm">
										<BadgeText>{scope}</BadgeText>
									</Badge>
								))}
							</HStack>
						) : (
							<Text size="sm" className="text-typography-500">
								No scopes
							</Text>
						)}
					</View>
				</VStack>
			) : (
				<Text size="sm" className="text-typography-500">
					Not logged in
				</Text>
			)}
		</Card>
	);
}

function InfoRow({
	label,
	value,
	selectable,
}: {
	label: string;
	value: string;
	selectable?: boolean;
}) {
	return (
		<HStack className="justify-between items-center">
			<Text size="sm" className="text-typography-500">
				{label}
			</Text>
			<Text size="sm" className="font-mono" selectable={selectable}>
				{value}
			</Text>
		</HStack>
	);
}

function RefreshTokensSection() {
	const queryClient = useQueryClient();
	const toast = useToast();

	const { data: tokenResponse, isLoading } = useQuery({
		queryKey: ["refresh-tokens"],
		queryFn: listRefreshTokens,
		retry: false,
	});

	const tokens = tokenResponse?.tokens ?? [];

	const revokeMutation = useMutation({
		mutationFn: revokeRefreshToken,
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["refresh-tokens"] });
			toast.show({
				id: "token-revoked",
				render: () => (
					<Toast action="success">
						<ToastTitle>Token revoked</ToastTitle>
					</Toast>
				),
			});
		},
	});

	const [revokeTarget, setRevokeTarget] = useState<RefreshTokenInfo | null>(
		null,
	);

	return (
		<Card className="p-5 gap-4">
			<HStack className="items-center gap-2">
				<KeyRound size={22} className="text-primary-500" />
				<Heading size="md">Refresh Tokens</Heading>
			</HStack>

			{isLoading ? (
				<Box className="py-4 items-center">
					<Spinner />
				</Box>
			) : tokens.length === 0 ? (
				<Text size="sm" className="text-typography-500">
					No active refresh tokens
				</Text>
			) : (
				<VStack className="gap-2">
					{tokens.map((token) => (
						<HStack
							key={token.id}
							className="justify-between items-center p-3 bg-background-50 rounded-lg"
						>
							<VStack className="gap-1 flex-1">
								<HStack className="items-center gap-2">
									<Text size="sm" className="font-mono">
										{token.id.slice(0, 8)}...
									</Text>
									<Badge
										action={token.is_active ? "success" : "error"}
										size="sm"
									>
										<BadgeText>
											{token.is_active ? "Active" : "Revoked"}
										</BadgeText>
									</Badge>
								</HStack>
								<HStack className="items-center gap-1">
									<Clock size={12} className="text-typography-400" />
									<Text size="xs" className="text-typography-400">
										Expires {new Date(token.expires_at).toLocaleDateString()}
									</Text>
								</HStack>
							</VStack>
							{token.is_active && (
								<Button
									size="xs"
									variant="outline"
									action="negative"
									onPress={() => setRevokeTarget(token)}
								>
									<ButtonIcon as={Trash2} />
								</Button>
							)}
						</HStack>
					))}
				</VStack>
			)}

			<ConfirmDialog
				open={!!revokeTarget}
				onOpenChange={(open) => !open && setRevokeTarget(null)}
				title="Revoke Token"
				description="Are you sure you want to revoke this refresh token? This will force logout on the device using this token."
				confirmText="Revoke"
				onConfirm={async () => {
					if (revokeTarget) {
						await revokeMutation.mutateAsync(revokeTarget.id);
					}
				}}
				destructive
			/>
		</Card>
	);
}

export default function SettingsPage() {
	const router = useRouter();
	const { logout } = useAuth();
	const [showLogout, setShowLogout] = useState(false);

	const handleLogout = async () => {
		await logout();
		router.replace("/login");
	};

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-3xl">
				<PageHeader
					title="Settings"
					description="Manage your account and sessions"
				/>

				<UserInfoSection />
				<RefreshTokensSection />

				<Divider />

				<Button action="negative" onPress={() => setShowLogout(true)}>
					<ButtonIcon as={LogOut} />
					<ButtonText>Logout</ButtonText>
				</Button>
			</Box>

			<ConfirmDialog
				open={showLogout}
				onOpenChange={setShowLogout}
				title="Logout"
				description="Are you sure you want to logout? You will need to sign in again."
				confirmText="Logout"
				onConfirm={handleLogout}
				destructive
			/>
		</ScrollView>
	);
}
