"use client";

import { Link, useRouter } from "expo-router";
import { useState } from "react";
import { ScrollView } from "react-native";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertText } from "@/components/ui/alert";
import { Box } from "@/components/ui/box";
import { Button, ButtonSpinner, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
	FormControl,
	FormControlHelper,
	FormControlHelperText,
	FormControlLabel,
	FormControlLabelText,
} from "@/components/ui/form-control";
import { HStack } from "@/components/ui/hstack";
import { Input, InputField } from "@/components/ui/input";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { useCreateUser } from "@/hooks/use-users";

export default function CreateUserPage() {
	const router = useRouter();
	const createMutation = useCreateUser();
	const toast = useToast();

	const [username, setUsername] = useState("");
	const [email, setEmail] = useState("");
	const [password, setPassword] = useState("");
	const [teamId, setTeamId] = useState("");
	const [role, setRole] = useState<"admin" | "member" | "readonly">("member");

	const handleCreate = async () => {
		if (!username.trim() || !email.trim() || password.length < 8) return;

		await createMutation.mutateAsync({
			username: username.trim(),
			email: email.trim(),
			password,
			team_id: teamId.trim() || undefined,
			role,
		});

		toast.show({
			id: "user-created",
			render: () => (
				<Toast action="success">
					<ToastTitle>User created</ToastTitle>
				</Toast>
			),
		});

		router.replace("/(tabs)/(admin)/users");
	};

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-2xl">
				<PageHeader
					title="Create User"
					description="Create a user and register their initial OPAQUE password"
				/>

				<Card className="p-6 gap-5">
					<FormControl isRequired>
						<FormControlLabel>
							<FormControlLabelText>Username</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField value={username} onChangeText={setUsername} />
						</Input>
					</FormControl>

					<FormControl isRequired>
						<FormControlLabel>
							<FormControlLabelText>Email</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField
								value={email}
								onChangeText={setEmail}
								keyboardType="email-address"
								autoCapitalize="none"
							/>
						</Input>
					</FormControl>

					<FormControl isRequired>
						<FormControlLabel>
							<FormControlLabelText>Initial Password</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField
								value={password}
								onChangeText={setPassword}
								secureTextEntry
							/>
						</Input>
						<FormControlHelper>
							<FormControlHelperText>
								Use at least 8 characters.
							</FormControlHelperText>
						</FormControlHelper>
					</FormControl>

					<FormControl>
						<FormControlLabel>
							<FormControlLabelText>Team ID</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField
								value={teamId}
								onChangeText={setTeamId}
								placeholder="Defaults to your current team"
							/>
						</Input>
					</FormControl>

					<FormControl>
						<FormControlLabel>
							<FormControlLabelText>Role</FormControlLabelText>
						</FormControlLabel>
						<HStack className="gap-2 flex-wrap">
							{(["member", "readonly", "admin"] as const).map((option) => (
								<Button
									key={option}
									size="sm"
									variant={role === option ? "solid" : "outline"}
									action="secondary"
									onPress={() => setRole(option)}
								>
									<ButtonText>{option}</ButtonText>
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
								: "Failed to create user"}
						</AlertText>
					</Alert>
				)}

				<HStack className="justify-end gap-3">
					<Link href="/(tabs)/(admin)/users" asChild>
						<Button variant="outline" action="secondary">
							<ButtonText>Cancel</ButtonText>
						</Button>
					</Link>
					<Button
						onPress={handleCreate}
						isDisabled={
							!username.trim() ||
							!email.trim() ||
							password.length < 8 ||
							createMutation.isPending
						}
					>
						{createMutation.isPending && <ButtonSpinner />}
						<ButtonText>
							{createMutation.isPending ? "Creating..." : "Create User"}
						</ButtonText>
					</Button>
				</HStack>
			</Box>
		</ScrollView>
	);
}
