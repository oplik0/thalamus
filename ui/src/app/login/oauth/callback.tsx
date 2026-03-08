import { useEffect, useState } from "react";
import { useRouter, useLocalSearchParams } from "expo-router";
import { setToken } from "@/lib/auth";
import { Text } from "@/components/ui/text";
import { Spinner } from "@/components/ui/spinner";
import { VStack } from "@/components/ui/vstack";
import { Center } from "@/components/ui/center";

export default function OAuthCallback() {
  const router = useRouter();
  // Backend redirects with token directly (no code exchange needed)
  const params = useLocalSearchParams<{
    token?: string;
    user_id?: string;
    team_id?: string;
    is_new_user?: string;
  }>();
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    async function processCallback() {
      try {
        const { token, user_id, team_id } = params;

        if (!token || !user_id || !team_id) {
          console.error("Missing OAuth callback params:", params);
          throw new Error("Missing OAuth callback parameters");
        }

        // Store the token directly (backend already did the code exchange)
        await setToken(token);

        // Success - redirect to home
        router.replace("/");
      } catch (err) {
        console.error("OAuth callback error:", err);
        setError(err instanceof Error ? err.message : "Authentication failed");
      }
    }

    processCallback();
  }, [params, router]);

  if (error) {
    return (
      <Center>
        <VStack space="lg">
          <Text className="text-red-500">{error}</Text>
          <Text onPress={() => router.replace("/login")}>
            Return to login
          </Text>
        </VStack>
      </Center>
    );
  }

  return (
    <Center>
      <VStack space="lg">
        <Spinner size="large" />
        <Text>Completing authentication...</Text>
      </VStack>
    </Center>
  );
}
