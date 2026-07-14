"use client";

import type { ReactNode } from "react";
import { View } from "react-native";
import { Heading } from "@/components/ui/heading";
import { Text } from "@/components/ui/text";

interface PageHeaderProps {
	title: string;
	description?: string;
	actions?: ReactNode;
}

export function PageHeader({ title, description, actions }: PageHeaderProps) {
	return (
		<View className="flex-row justify-between items-start gap-4">
			<View className="gap-1 flex-1">
				<Heading size="2xl">{title}</Heading>
				{description && (
					<Text size="sm" className="text-typography-500">
						{description}
					</Text>
				)}
			</View>
			{actions && <View className="flex-row gap-2">{actions}</View>}
		</View>
	);
}
