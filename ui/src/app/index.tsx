import { Platform, StyleSheet, View } from "react-native";
import { SafeAreaView } from "react-native-safe-area-context";
import { ThemedText } from "@/components/themed-text";
import { ThemedView } from "@/components/themed-view";
import { BottomTabInset, MaxContentWidth, Spacing } from "@/constants/theme";
import { useHealthCheck } from "@/hooks/use-health-check";

function HealthStatus() {
	const { data, isLoading, isError } = useHealthCheck();

	if (isLoading) {
		return (
			<View style={styles.statusBadge}>
				<View style={[styles.statusDot, styles.statusDotLoading]} />
				<ThemedText type="small">Checking backend...</ThemedText>
			</View>
		);
	}

	if (isError || !data) {
		return (
			<View style={styles.statusBadge}>
				<View style={[styles.statusDot, styles.statusDotError]} />
				<ThemedText type="small">Backend offline</ThemedText>
			</View>
		);
	}

	return (
		<View style={styles.statusBadge}>
			<View style={[styles.statusDot, styles.statusDotHealthy]} />
			<ThemedText type="small">Backend healthy</ThemedText>
		</View>
	);
}

export default function HomeScreen() {
	return (
		<ThemedView style={styles.container}>
			<SafeAreaView style={styles.safeArea}>
				<ThemedView style={styles.heroSection}>
					<ThemedText type="title" style={styles.title}>
						Thalmus
					</ThemedText>
					<ThemedText type="subtitle" style={styles.subtitle}>
						LLM Router & Load Balancer
					</ThemedText>
				</ThemedView>

				<HealthStatus />

				<ThemedView type="backgroundElement" style={styles.infoCard}>
					<ThemedText type="smallBold">Getting Started</ThemedText>
					<ThemedText type="small">
						This is the Thalmus management UI. Edit{" "}
						<ThemedText type="code">src/app/index.tsx</ThemedText> to start
						building.
					</ThemedText>
					{Platform.OS === "web" && (
						<ThemedText type="small">
							API endpoint:{" "}
							<ThemedText type="code">
								{process.env.EXPO_PUBLIC_API_URL ?? "http://localhost:3000"}
							</ThemedText>
						</ThemedText>
					)}
				</ThemedView>
			</SafeAreaView>
		</ThemedView>
	);
}

const styles = StyleSheet.create({
	container: {
		flex: 1,
		justifyContent: "center",
		flexDirection: "row",
	},
	safeArea: {
		flex: 1,
		paddingHorizontal: Spacing.four,
		alignItems: "center",
		gap: Spacing.three,
		paddingBottom: BottomTabInset + Spacing.three,
		maxWidth: MaxContentWidth,
	},
	heroSection: {
		alignItems: "center",
		justifyContent: "center",
		flex: 1,
		paddingHorizontal: Spacing.four,
		gap: Spacing.two,
	},
	title: {
		textAlign: "center",
	},
	subtitle: {
		textAlign: "center",
		opacity: 0.7,
	},
	statusBadge: {
		flexDirection: "row",
		alignItems: "center",
		gap: 8,
		paddingHorizontal: 16,
		paddingVertical: 8,
		borderRadius: 20,
	},
	statusDot: {
		width: 10,
		height: 10,
		borderRadius: 5,
	},
	statusDotHealthy: {
		backgroundColor: "#22c55e",
	},
	statusDotError: {
		backgroundColor: "#ef4444",
	},
	statusDotLoading: {
		backgroundColor: "#eab308",
	},
	infoCard: {
		gap: Spacing.two,
		alignSelf: "stretch",
		paddingHorizontal: Spacing.three,
		paddingVertical: Spacing.four,
		borderRadius: Spacing.four,
	},
});
