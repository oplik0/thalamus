"use client";

import { useMutation, useQuery } from "@tanstack/react-query";
import { Redirect, useRouter } from "expo-router";
import { AlertCircle, KeyRound, LogIn, User } from "lucide-react-native";
import { useState } from "react";
import { View } from "react-native";
import { SafeAreaView } from "react-native-safe-area-context";
import { Alert, AlertText } from "@/components/ui/alert";
import { Button, ButtonSpinner, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Center } from "@/components/ui/center";
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
import { getProviders, getSetupStatus, startOAuthFlow } from "@/services/auth";

export default function LoginScreen() {
	const router = useRouter();
	const { refetchUser, loginWithCredentials, isAuthenticated } = useAuth();
	const [error, setError] = useState<string | null>(null);
	const [username, setUsername] = useState("");
	const [password, setPassword] = useState("");

	// Check whether first-run setup is required
	const { data: setupStatus, isLoading: isLoadingSetup } = useQuery({
		queryKey: ["setup-status"],
		queryFn: getSetupStatus,
		retry: false,
	});

	// Fetch OAuth providers
	const {
		data: providers,
		isLoading: isLoadingProviders,
		isError: isProvidersError,
	} = useQuery({
		queryKey: ["oauth-providers"],
		queryFn: getProviders,
		retry: 2,
	});

	// OAuth login mutation
	const oauthMutation = useMutation({
		mutationFn: async (providerName: string) => {
			const result = await startOAuthFlow(providerName);
			await refetchUser();
			return result;
		},
		onSuccess: () => {
			router.replace("/(tabs)/(admin)");
		},
		onError: (err: Error) => {
			setError(err.message || "Login failed. Please try again.");
		},
	});

	// Credentials login mutation
	const credentialsMutation = useMutation({
		mutationFn: async () => {
			await loginWithCredentials(username, password);
		},
		onSuccess: () => {
			router.replace("/(tabs)/(admin)");
		},
		onError: (err: Error) => {
			setError(err.message || "Invalid username or password.");
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

	// First-run setup takes precedence over everything else
	if (setupStatus?.needs_setup) {
		return <Redirect href="/login/setup" />;
	}

	const handleOAuthLogin = (providerName: string) => {
		setError(null);
		oauthMutation.mutate(providerName);
	};

	const handleCredentialsLogin = () => {
		setError(null);
		if (!username.trim() || !password) {
			setError("Please enter both username and password.");
			return;
		}
		credentialsMutation.mutate();
	};

	const hasProviders = providers && providers.length > 0;

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
							LLM Router & Load Balancer
						</Text>
					</VStack>

					{/* Login card */}
					<Card className="w-full p-6 gap-5">
						<Heading size="md" className="text-center">
							Sign in to continue
						</Heading>

						{error && (
							<Alert action="error">
								<AlertCircle size={16} className="text-error-600" />
								<AlertText>{error}</AlertText>
							</Alert>
						)}

						{/* Username / Password */}
						<VStack className="gap-3">
							<FormControl>
								<FormControlLabel>
									<FormControlLabelText>Username</FormControlLabelText>
								</FormControlLabel>
								<Input>
									<InputIcon as={User} />
									<InputField
										placeholder="Enter your username"
										value={username}
										onChangeText={setUsername}
										autoCapitalize="none"
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
										placeholder="Enter your password"
										value={password}
										onChangeText={setPassword}
										secureTextEntry
									/>
								</Input>
							</FormControl>

							<Button
								size="lg"
								onPress={handleCredentialsLogin}
								isDisabled={credentialsMutation.isPending}
							>
								{credentialsMutation.isPending ? (
									<ButtonSpinner />
								) : (
									<LogIn size={18} className="text-typography-0" />
								)}
								<ButtonText>Sign in</ButtonText>
							</Button>
						</VStack>

						{hasProviders && (
							<View className="flex-row items-center gap-3">
								<View className="h-px flex-1 bg-outline-200" />
								<Text size="xs" className="text-typography-500">
									OR
								</Text>
								<View className="h-px flex-1 bg-outline-200" />
							</View>
						)}

						{/* OAuth providers */}
						{isLoadingProviders ? (
							<Center className="py-8">
								<Spinner size="large" />
							</Center>
						) : isProvidersError ? (
							<VStack className="gap-2 items-center py-4">
								<Text size="sm" className="text-typography-500 text-center">
									Could not load login providers. Is the backend running?
								</Text>
							</VStack>
						) : hasProviders ? (
							<VStack className="gap-3">
								{providers.map((provider) => (
									<Button
										key={provider.name}
										variant="outline"
										action="secondary"
										size="lg"
										onPress={() => handleOAuthLogin(provider.name)}
										isDisabled={oauthMutation.isPending}
									>
										{oauthMutation.isPending &&
										oauthMutation.variables === provider.name ? (
											<ButtonSpinner />
										) : (
											<LogIn size={18} className="text-typography-600" />
										)}
										<ButtonText>Continue with {provider.name}</ButtonText>
									</Button>
								))}
							</VStack>
						) : null}
					</Card>
				</VStack>
			</SafeAreaView>
		</View>
	);
}
