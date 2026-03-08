import {
	DarkTheme,
	DefaultTheme,
	ThemeProvider,
} from "@react-navigation/native";
import { Stack } from "expo-router";
import { QueryClientProvider } from "@tanstack/react-query";
import { useColorScheme } from "@/hooks/use-color-scheme";

import { GluestackUIProvider } from "@/components/ui/gluestack-ui-provider";
import { queryClient } from "@/lib/query-client";
import { AuthProvider } from "@/contexts/auth-context";

import "@/global.css";

export default function RootLayout() {
	const colorScheme = useColorScheme();
	return (
		<QueryClientProvider client={queryClient}>
			<GluestackUIProvider mode={colorScheme === "dark" ? "dark" : "light"}>
				<ThemeProvider
					value={colorScheme === "dark" ? DarkTheme : DefaultTheme}
				>
					<AuthProvider>
						<Stack
							screenOptions={{
								headerShown: false,
							}}
						>
							<Stack.Screen name="(tabs)" options={{ headerShown: false }} />
							<Stack.Screen name="login" options={{ headerShown: false }} />
						</Stack>
					</AuthProvider>
				</ThemeProvider>
			</GluestackUIProvider>
		</QueryClientProvider>
	);
}
