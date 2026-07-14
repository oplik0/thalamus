"use client";

import { Link } from "expo-router";
import { Clock, FileKey2, Hash, Plus, Trash2 } from "lucide-react-native";
import { useState } from "react";
import { ScrollView, View } from "react-native";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { EmptyState } from "@/components/empty-state";
import { PageHeader } from "@/components/page-header";
import { deriveKeyStatus, StatusBadge } from "@/components/status-badge";
import { Badge, BadgeText } from "@/components/ui/badge";
import { Box } from "@/components/ui/box";
import { Button, ButtonIcon, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { HStack } from "@/components/ui/hstack";
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { VStack } from "@/components/ui/vstack";
import { useRevokeSigningKey, useSigningKeys } from "@/hooks/use-signing-keys";
import type { SigningKeyInfo } from "@/lib/types";

function SigningKeyRow({
	signingKey,
	onRevoke,
}: {
	signingKey: SigningKeyInfo;
	onRevoke: () => void;
}) {
	const status = deriveKeyStatus(signingKey);

	return (
		<Card className="p-4">
			<HStack className="justify-between items-start">
				<VStack className="flex-1 gap-1.5">
					<HStack className="items-center gap-2">
						<Text className="font-semibold">
							{signingKey.name ?? signingKey.key_id}
						</Text>
						<StatusBadge status={status} />
					</HStack>

					<HStack className="gap-2 items-center">
						<Badge action="info" size="sm">
							<BadgeText>{signingKey.algorithm}</BadgeText>
						</Badge>
						<Text size="xs" className="text-typography-500 font-mono">
							{signingKey.fingerprint.slice(0, 24)}...
						</Text>
					</HStack>

					{signingKey.scopes && signingKey.scopes.length > 0 && (
						<HStack className="gap-2 flex-wrap mt-1">
							{signingKey.scopes.map((scope) => (
								<Badge key={scope} action="muted" size="sm">
									<BadgeText>{scope}</BadgeText>
								</Badge>
							))}
						</HStack>
					)}

					<HStack className="gap-4 mt-1">
						<HStack className="items-center gap-1">
							<Hash size={12} className="text-typography-400" />
							<Text size="xs" className="text-typography-400">
								Used {signingKey.use_count} times
							</Text>
						</HStack>
						<Text size="xs" className="text-typography-400">
							Created {new Date(signingKey.created_at).toLocaleDateString()}
						</Text>
						{signingKey.expires_at && (
							<HStack className="items-center gap-1">
								<Clock size={12} className="text-typography-400" />
								<Text size="xs" className="text-typography-400">
									Expires {new Date(signingKey.expires_at).toLocaleDateString()}
								</Text>
							</HStack>
						)}
					</HStack>
				</VStack>

				{signingKey.is_active && (
					<Button
						size="xs"
						variant="outline"
						action="negative"
						onPress={onRevoke}
						accessibilityLabel="Revoke signing key"
					>
						<ButtonIcon as={Trash2} />
					</Button>
				)}
			</HStack>
		</Card>
	);
}

export default function SigningKeysPage() {
	const { data: signingKeys, isLoading } = useSigningKeys();
	const revokeMutation = useRevokeSigningKey();
	const toast = useToast();

	const [revokeTarget, setRevokeTarget] = useState<SigningKeyInfo | null>(null);

	const handleRevoke = async () => {
		if (!revokeTarget) return;
		await revokeMutation.mutateAsync(revokeTarget.key_id);
		toast.show({
			id: `revoke-${revokeTarget.key_id}`,
			render: () => (
				<Toast action="success">
					<ToastTitle>Signing key revoked</ToastTitle>
				</Toast>
			),
		});
	};

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-5xl">
				<PageHeader
					title="Signing Keys"
					description="Manage HTTP signature keys for request authentication"
					actions={
						<Link href="/(tabs)/(admin)/signing-keys/create" asChild>
							<Button size="sm">
								<ButtonIcon as={Plus} />
								<ButtonText>Create Key</ButtonText>
							</Button>
						</Link>
					}
				/>

				{isLoading ? (
					<Box className="py-12 items-center">
						<Spinner size="large" />
					</Box>
				) : !signingKeys || signingKeys.length === 0 ? (
					<EmptyState
						icon={<FileKey2 size={32} className="text-typography-400" />}
						title="No signing keys yet"
						description="Create your first signing key to enable HTTP signature authentication"
					/>
				) : (
					<VStack className="gap-3">
						{signingKeys.map((key) => (
							<SigningKeyRow
								key={key.id}
								signingKey={key}
								onRevoke={() => setRevokeTarget(key)}
							/>
						))}
					</VStack>
				)}
			</Box>

			<ConfirmDialog
				open={!!revokeTarget}
				onOpenChange={(open) => !open && setRevokeTarget(null)}
				title="Revoke Signing Key"
				description={`Are you sure you want to revoke "${revokeTarget?.name ?? revokeTarget?.key_id}"? This action cannot be undone.`}
				confirmText="Revoke"
				onConfirm={handleRevoke}
				destructive
			/>
		</ScrollView>
	);
}
