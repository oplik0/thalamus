"use client";

import { Link } from "expo-router";
import {
	ArrowRight,
	CheckCircle2,
	Loader2,
	XCircle,
} from "lucide-react-native";
import { Platform, View } from "react-native";
import { SafeAreaView } from "react-native-safe-area-context";
import { Badge, BadgeText } from "@/components/ui/badge";
import { Button, ButtonIcon, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Center } from "@/components/ui/center";
import { Heading } from "@/components/ui/heading";
import { HStack } from "@/components/ui/hstack";
import { Text } from "@/components/ui/text";
import { VStack } from "@/components/ui/vstack";
import { useHealthCheck } from "@/hooks/use-health-check";

function HealthStatus() {
	const { data, isLoading, isError } = useHealthCheck();

	return (
		<HStack className="items-center gap-2 px-4 py-2 rounded-full bg-background-50">
			{isLoading ? (
				<Loader2 size={16} className="text-warning-500" />
			) : isError ? (
				<XCircle size={16} className="text-error-500" />
			) : (
				<CheckCircle2 size={16} className="text-success-500" />
			)}
			<Text size="sm">
				{isLoading
					? "Checking backend..."
					: isError
						? "Backend offline"
						: "Backend healthy"}
			</Text>
			{data?.version && (
				<Badge action="info" size="sm">
					<BadgeText>v{data.version}</BadgeText>
				</Badge>
			)}
		</HStack>
	);
}

export default function HomeScreen() {
	return (
		<View className="flex-1 bg-background-0">
			<SafeAreaView style={{ flex: 1 }}>
				<Center className="flex-1 px-6 gap-6">
					<VStack className="items-center gap-2">
						<Heading size="3xl">Thalamus</Heading>
						<Text size="lg" className="text-typography-500">
							LLM Router & Load Balancer
						</Text>
					</VStack>

					<HealthStatus />

					<Link href="/(tabs)/(admin)" asChild>
						<Button size="lg">
							<ButtonText>Go to Admin Dashboard</ButtonText>
							<ButtonIcon as={ArrowRight} />
						</Button>
					</Link>

					{Platform.OS === "web" && (
						<Card className="w-full max-w-md p-4 gap-2">
							<Text size="sm" className="font-semibold">
								Getting Started
							</Text>
							<Text size="sm" className="text-typography-500">
								API endpoint:{" "}
								<Text size="sm" className="font-mono">
									{process.env.EXPO_PUBLIC_API_URL ?? "http://localhost:3000"}
								</Text>
							</Text>
						</Card>
					)}
				</Center>
			</SafeAreaView>
		</View>
	);
}
