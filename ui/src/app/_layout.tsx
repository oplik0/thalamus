import {
	DarkTheme,
	DefaultTheme,
	ThemeProvider,
} from "@react-navigation/native";
import { QueryClientProvider } from "@tanstack/react-query";
import { useColorScheme } from "react-native";

import { AnimatedSplashOverlay } from "@/components/animated-icon";
import AppTabs from "@/components/app-tabs";
import { GluestackUIProvider } from "@/components/ui/gluestack-ui-provider";
import { queryClient } from "@/lib/query-client";

import "@/global.css";

export default function TabLayout() {
	const colorScheme = useColorScheme();
	return (
		<QueryClientProvider client={queryClient}>
			<GluestackUIProvider mode={colorScheme === "dark" ? "dark" : "light"}>
				<ThemeProvider
					value={colorScheme === "dark" ? DarkTheme : DefaultTheme}
				>
					<AnimatedSplashOverlay />
					<AppTabs />
				</ThemeProvider>
			</GluestackUIProvider>
		</QueryClientProvider>
	);
}
