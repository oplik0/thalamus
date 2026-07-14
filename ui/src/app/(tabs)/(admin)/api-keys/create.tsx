"use client";

import { Link, useRouter } from "expo-router";
import { AlertTriangle, CheckIcon, ShieldCheck } from "lucide-react-native";
import { useState } from "react";
import { ScrollView, View } from "react-native";
import { CopyButton } from "@/components/copy-button";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertText } from "@/components/ui/alert";
import { Box } from "@/components/ui/box";
import { Button, ButtonSpinner, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
	Checkbox,
	CheckboxGroup,
	CheckboxIcon,
	CheckboxIndicator,
	CheckboxLabel,
} from "@/components/ui/checkbox";
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
import { Text } from "@/components/ui/text";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { VStack } from "@/components/ui/vstack";
import { useCreateApiKey } from "@/hooks/use-api-keys";
import type { CreateApiKeyResponse } from "@/lib/types";

const AVAILABLE_SCOPES = [
	"api_keys:create",
	"api_keys:read",
	"api_keys:revoke",
	"api_keys:rotate",
	"tokens:create",
	"tokens:read",
	"tokens:revoke",
	"signing_keys:create",
	"signing_keys:read",
	"signing_keys:revoke",
	"oauth:link",
	"oauth:unlink",
	"admin",
];

const EXPIRY_OPTIONS = [
	{ label: "Never", value: undefined },
	{ label: "30 days", value: 30 },
	{ label: "90 days", value: 90 },
	{ label: "180 days", value: 180 },
	{ label: "1 year", value: 365 },
];

export default function CreateApiKeyPage() {
	const router = useRouter();
	const createMutation = useCreateApiKey();
	const toast = useToast();

	const [name, setName] = useState("");
	const [description, setDescription] = useState("");
	const [selectedScopes, setSelectedScopes] = useState<string[]>([]);
	const [expiresInDays, setExpiresInDays] = useState<number | undefined>(
		undefined,
	);
	const [createdKey, setCreatedKey] = useState<CreateApiKeyResponse | null>(
		null,
	);

	const handleCreate = async () => {
		if (!name.trim()) return;

		const result = await createMutation.mutateAsync({
			name: name.trim(),
			description: description.trim() || undefined,
			scopes: selectedScopes.length > 0 ? selectedScopes : undefined,
			expires_in_days: expiresInDays,
		});

		setCreatedKey(result);
		toast.show({
			id: "api-key-created",
			render: () => (
				<Toast action="success">
					<ToastTitle>API key created successfully</ToastTitle>
				</Toast>
			),
		});
	};

	// Key created - show secret
	if (createdKey) {
		return (
			<ScrollView className="flex-1 bg-background-0">
				<Box className="p-6 gap-6 max-w-2xl">
					<PageHeader title="API Key Created" />

					<Card className="p-6 gap-4">
						<HStack className="items-center gap-2">
							<ShieldCheck size={28} className="text-success-500" />
							<Heading size="md">Save this key securely</Heading>
						</HStack>

						<Alert action="warning">
							<AlertTriangle size={16} className="text-warning-600" />
							<AlertText>
								This is the only time you will see this key. Copy it now and
								store it in a secure location.
							</AlertText>
						</Alert>

						<VStack className="gap-2">
							<Text size="sm" className="font-semibold">
								API Key
							</Text>
							<HStack className="items-center gap-2 bg-background-50 p-3 rounded-lg">
								<Text
									size="sm"
									className="flex-1 font-mono break-all"
									selectable
								>
									{createdKey.key}
								</Text>
								<CopyButton value={createdKey.key} />
							</HStack>
						</VStack>

						<VStack className="gap-1">
							<Text size="xs" className="text-typography-500">
								Name: {createdKey.name}
							</Text>
							<Text size="xs" className="text-typography-500">
								Prefix: {createdKey.key_prefix}
							</Text>
							{createdKey.expires_at && (
								<Text size="xs" className="text-typography-500">
									Expires:{" "}
									{new Date(createdKey.expires_at).toLocaleDateString()}
								</Text>
							)}
						</VStack>
					</Card>

					<HStack className="justify-end">
						<Link href="/(tabs)/(admin)/api-keys" asChild>
							<Button>
								<ButtonText>Done</ButtonText>
							</Button>
						</Link>
					</HStack>
				</Box>
			</ScrollView>
		);
	}

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-2xl">
				<PageHeader
					title="Create API Key"
					description="Generate a new API key for authenticating with the Thalamus API"
				/>

				<Card className="p-6 gap-5">
					<FormControl isRequired>
						<FormControlLabel>
							<FormControlLabelText>Name</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField
								value={name}
								onChangeText={setName}
								placeholder="e.g. Production API Key"
							/>
						</Input>
					</FormControl>

					<FormControl>
						<FormControlLabel>
							<FormControlLabelText>Description</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField
								value={description}
								onChangeText={setDescription}
								placeholder="Optional description"
							/>
						</Input>
					</FormControl>

					<Divider />

					<FormControl>
						<FormControlLabel>
							<FormControlLabelText>Scopes</FormControlLabelText>
						</FormControlLabel>
						<FormControlHelper>
							<FormControlHelperText>
								Select the permissions for this API key. Leave empty for default
								scopes.
							</FormControlHelperText>
						</FormControlHelper>
						<CheckboxGroup
							value={selectedScopes}
							onChange={setSelectedScopes}
							className="mt-2"
						>
							<View className="flex-row flex-wrap gap-3">
								{AVAILABLE_SCOPES.map((scope) => (
									<Checkbox key={scope} value={scope} size="sm">
										<CheckboxIndicator>
											<CheckboxIcon as={CheckIcon} />
										</CheckboxIndicator>
										<CheckboxLabel>{scope}</CheckboxLabel>
									</Checkbox>
								))}
							</View>
						</CheckboxGroup>
					</FormControl>

					<Divider />

					<FormControl>
						<FormControlLabel>
							<FormControlLabelText>Expiration</FormControlLabelText>
						</FormControlLabel>
						<HStack className="gap-2 flex-wrap">
							{EXPIRY_OPTIONS.map((option) => (
								<Button
									key={option.label}
									size="sm"
									variant={expiresInDays === option.value ? "solid" : "outline"}
									action={
										expiresInDays === option.value ? "primary" : "secondary"
									}
									onPress={() => setExpiresInDays(option.value)}
								>
									<ButtonText>{option.label}</ButtonText>
								</Button>
							))}
						</HStack>
					</FormControl>
				</Card>

				{createMutation.error && (
					<Alert action="error">
						<AlertText>
							{createMutation.error instanceof Error
								? createMutation.error.message
								: "Failed to create API key"}
						</AlertText>
					</Alert>
				)}

				<HStack className="justify-end gap-3">
					<Link href="/(tabs)/(admin)/api-keys" asChild>
						<Button variant="outline" action="secondary">
							<ButtonText>Cancel</ButtonText>
						</Button>
					</Link>
					<Button
						onPress={handleCreate}
						isDisabled={!name.trim() || createMutation.isPending}
					>
						{createMutation.isPending && <ButtonSpinner />}
						<ButtonText>
							{createMutation.isPending ? "Creating..." : "Create Key"}
						</ButtonText>
					</Button>
				</HStack>
			</Box>
		</ScrollView>
	);
}
