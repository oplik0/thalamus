"use client";

import type { ReactNode } from "react";
import { View } from "react-native";
import { Box } from "@/components/ui/box";
import { Button, ButtonText } from "@/components/ui/button";
import { Heading } from "@/components/ui/heading";
import { Text } from "@/components/ui/text";

interface EmptyStateProps {
	icon?: ReactNode;
	title: string;
	description?: string;
	action?: {
		label: string;
		onPress: () => void;
	};
}

export function EmptyState({
	icon,
	title,
	description,
	action,
}: EmptyStateProps) {
	return (
		<Box className="flex-1 items-center justify-center py-16 px-6 gap-3">
			{icon && (
				<View className="w-16 h-16 rounded-full bg-background-100 items-center justify-center mb-2">
					{icon}
				</View>
			)}
			<Heading size="md" className="text-center">
				{title}
			</Heading>
			{description && (
				<Text
					size="sm"
					className="text-typography-500 text-center max-w-[300px]"
				>
					{description}
				</Text>
			)}
			{action && (
				<Button onPress={action.onPress} className="mt-3">
					<ButtonText>{action.label}</ButtonText>
				</Button>
			)}
		</Box>
	);
}
