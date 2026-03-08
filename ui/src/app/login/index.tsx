"use client";

import { useState } from "react";
import { View } from "react-native";
import { useRouter } from "expo-router";
import { useQuery, useMutation } from "@tanstack/react-query";
import { SafeAreaView } from "react-native-safe-area-context";
import { Card } from "@/components/ui/card";
import { Text } from "@/components/ui/text";
import { Heading } from "@/components/ui/heading";
import { Button, ButtonText, ButtonSpinner } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { VStack } from "@/components/ui/vstack";
import { Center } from "@/components/ui/center";
import { Alert, AlertText } from "@/components/ui/alert";
import { getProviders, startOAuthFlow } from "@/services/auth";
import { useAuth } from "@/contexts/auth-context";
import { LogIn, AlertCircle } from "lucide-react-native";

export default function LoginScreen() {
  const router = useRouter();
  const { refetchUser } = useAuth();
  const [error, setError] = useState<string | null>(null);

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
  const loginMutation = useMutation({
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

  const handleLogin = (providerName: string) => {
    setError(null);
    loginMutation.mutate(providerName);
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
            ) : providers && providers.length > 0 ? (
              <VStack className="gap-3">
                {providers.map((provider) => (
                  <Button
                    key={provider.name}
                    variant="outline"
                    action="secondary"
                    size="lg"
                    onPress={() => handleLogin(provider.name)}
                    isDisabled={loginMutation.isPending}
                  >
                    {loginMutation.isPending &&
                    loginMutation.variables === provider.name ? (
                      <ButtonSpinner />
                    ) : (
                      <LogIn size={18} className="text-typography-600" />
                    )}
                    <ButtonText>
                      Continue with {provider.name}
                    </ButtonText>
                  </Button>
                ))}
              </VStack>
            ) : (
              <Text
                size="sm"
                className="text-typography-500 text-center py-4"
              >
                No OAuth providers configured. Contact your administrator.
              </Text>
            )}
          </Card>
        </VStack>
      </SafeAreaView>
    </View>
  );
}
