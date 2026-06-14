import {
	DarkTheme,
	DefaultTheme,
	ThemeProvider,
} from "@react-navigation/native";
import { QueryClientProvider } from "@tanstack/react-query";
import { Stack } from "expo-router";
import { GluestackUIProvider } from "@/components/ui/gluestack-ui-provider";
import { AuthProvider } from "@/contexts/auth-context";
import { useColorScheme } from "@/hooks/use-color-scheme";
import { queryClient } from "@/lib/query-client";

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
