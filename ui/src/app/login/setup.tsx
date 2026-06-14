"use client";

import { useMutation, useQuery } from "@tanstack/react-query";
import { Redirect, useRouter } from "expo-router";
import {
	AlertCircle,
	CheckCircle2,
	KeyRound,
	Mail,
	User,
} from "lucide-react-native";
import { useState } from "react";
import { View } from "react-native";
import { SafeAreaView } from "react-native-safe-area-context";
import { Alert, AlertText } from "@/components/ui/alert";
import { Button, ButtonSpinner, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
	FormControl,
	FormControlLabel,
	FormControlLabelText,
} from "@/components/ui/form-control";
import { Heading } from "@/components/ui/heading";
import { Input, InputField, InputIcon } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { Text } from "@/components/ui/text";
import { VStack } from "@/components/ui/vstack";
import { useAuth } from "@/contexts/auth-context";
import { getSetupStatus, setupAccount } from "@/services/auth";

export default function SetupScreen() {
	const router = useRouter();
	const { refetchUser, isAuthenticated } = useAuth();
	const [error, setError] = useState<string | null>(null);
	const [username, setUsername] = useState("");
	const [email, setEmail] = useState("");
	const [password, setPassword] = useState("");
	const [confirmPassword, setConfirmPassword] = useState("");

	const { data: setupStatus, isLoading: isLoadingSetup } = useQuery({
		queryKey: ["setup-status"],
		queryFn: getSetupStatus,
		retry: false,
	});

	const setupMutation = useMutation({
		mutationFn: async () => {
			if (password !== confirmPassword) {
				throw new Error("Passwords do not match");
			}
			if (password.length < 8) {
				throw new Error("Password must be at least 8 characters");
			}
			return setupAccount(username, email, password);
		},
		onSuccess: () => {
			refetchUser();
			router.replace("/(tabs)/(admin)");
		},
		onError: (err: Error) => {
			setError(err.message || "Setup failed. Please try again.");
		},
	});

	if (isAuthenticated) {
		return <Redirect href="/(tabs)/(admin)" />;
	}

	if (isLoadingSetup) {
		return (
			<View className="flex-1 bg-background-0 items-center justify-center">
				<Spinner size="large" />
			</View>
		);
	}

	// If setup is no longer required, send the user to the normal login screen
	if (setupStatus && !setupStatus.needs_setup) {
		return <Redirect href="/login" />;
	}

	const handleSetup = () => {
		setError(null);
		if (!username.trim() || !email.trim() || !password) {
			setError("Please fill in all fields.");
			return;
		}
		setupMutation.mutate();
	};

	return (
		<View className="flex-1 bg-background-0">
			<SafeAreaView
				style={{ flex: 1 }}
				className="items-center justify-center px-6"
			>
				<VStack className="w-full max-w-sm gap-8 items-center">
					{/* Branding */}
					<VStack className="items-center gap-2">
						<Heading size="2xl">Thalamus</Heading>
						<Text size="md" className="text-typography-500">
							Initial setup
						</Text>
					</VStack>

					<Card className="w-full p-6 gap-5">
						<VStack className="items-center gap-2">
							<CheckCircle2 size={32} className="text-success-500" />
							<Heading size="md" className="text-center">
								Create admin account
							</Heading>
							<Text size="sm" className="text-typography-500 text-center">
								No authentication is configured yet. Create the first admin user
								to continue.
							</Text>
						</VStack>

						{error && (
							<Alert action="error">
								<AlertCircle size={16} className="text-error-600" />
								<AlertText>{error}</AlertText>
							</Alert>
						)}

						<VStack className="gap-3">
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Username</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputIcon as={User} />
									<InputField
										placeholder="admin"
										value={username}
										onChangeText={setUsername}
										autoCapitalize="none"
									/>
								</Input>
							</FormControl>

							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Email</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputIcon as={Mail} />
									<InputField
										placeholder="admin@example.com"
										value={email}
										onChangeText={setEmail}
										autoCapitalize="none"
										keyboardType="email-address"
									/>
								</Input>
							</FormControl>

							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Password</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputIcon as={KeyRound} />
									<InputField
										placeholder="At least 8 characters"
										value={password}
										onChangeText={setPassword}
										secureTextEntry
									/>
								</Input>
							</FormControl>

							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Confirm password</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputIcon as={KeyRound} />
									<InputField
										placeholder="Repeat your password"
										value={confirmPassword}
										onChangeText={setConfirmPassword}
										secureTextEntry
									/>
								</Input>
							</FormControl>

							<Button
								size="lg"
								onPress={handleSetup}
								isDisabled={setupMutation.isPending}
							>
								{setupMutation.isPending ? (
									<ButtonSpinner />
								) : (
									<CheckCircle2 size={18} className="text-typography-0" />
								)}
								<ButtonText>Create account</ButtonText>
							</Button>
						</VStack>
					</Card>
				</VStack>
			</SafeAreaView>
		</View>
	);
}
