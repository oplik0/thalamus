"use client";

import { Redirect, Stack } from "expo-router";
import { ActivityIndicator, StyleSheet, View } from "react-native";
import { useAuth } from "@/contexts/auth-context";

interface AuthGuardProps {
	/** The child components to render if authenticated */
	children: React.ReactNode;
}

/**
 * AuthGuard - Protected route wrapper that checks authentication
 *
 * This component:
 * - Shows a loading spinner while checking auth state
 * - Redirects to login if not authenticated
 * - Renders children if authenticated
 */
export function AuthGuard({ children }: AuthGuardProps) {
	const { isAuthenticated, isLoading } = useAuth();

	// Show loading spinner while checking auth state
	if (isLoading) {
		return (
			<View style={styles.loadingContainer}>
				<ActivityIndicator size="large" />
			</View>
		);
	}

	// Redirect to login if not authenticated
	if (!isAuthenticated) {
		return <Redirect href="/login" />;
	}

	// Render children if authenticated
	return <>{children}</>;
}

/**
 * AuthGuard with Stack - Wraps children in a Stack navigator for protected routes
 * Use this for pages that need a stack navigator (headers, etc.)
 */
export function AuthGuardStack({ children }: AuthGuardProps) {
	const { isAuthenticated, isLoading } = useAuth();

	if (isLoading) {
		return (
			<View style={styles.loadingContainer}>
				<ActivityIndicator size="large" />
			</View>
		);
	}

	if (!isAuthenticated) {
		return <Redirect href="/login" />;
	}

	return (
		<Stack
			screenOptions={{
				headerShown: false,
			}}
		>
			{children}
		</Stack>
	);
}

/**
 * Conditional wrapper that only renders children if authenticated
 * Use this for conditional UI elements (not route protection)
 */
export function IfAuthenticated({ children }: { children: React.ReactNode }) {
	const { isAuthenticated } = useAuth();

	if (!isAuthenticated) {
		return null;
	}

	return <>{children}</>;
}

/**
 * Conditional wrapper that renders children if NOT authenticated
 * Use this for showing login prompts or alternative content
 */
export function IfNotAuthenticated({
	children,
}: {
	children: React.ReactNode;
}) {
	const { isAuthenticated } = useAuth();

	if (isAuthenticated) {
		return null;
	}

	return <>{children}</>;
}

const styles = StyleSheet.create({
	loadingContainer: {
		flex: 1,
		justifyContent: "center",
		alignItems: "center",
		backgroundColor: "transparent",
	},
});
