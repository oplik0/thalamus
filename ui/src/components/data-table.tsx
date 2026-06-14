"use client";

import type { ReactNode } from "react";
import { Pressable, ScrollView, View } from "react-native";
import { Text } from "@/components/ui/text";

interface Column<T> {
	key: keyof T | string;
	title: string;
	width?: number;
	render?: (item: T) => ReactNode;
}

interface DataTableProps<T> {
	columns: Column<T>[];
	data: T[];
	keyExtractor: (item: T) => string;
	onRowPress?: (item: T) => void;
	emptyMessage?: string;
}

export function DataTable<T>({
	columns,
	data,
	keyExtractor,
	onRowPress,
	emptyMessage = "No data available",
}: DataTableProps<T>) {
	if (data.length === 0) {
		return (
			<View className="py-12 items-center">
				<Text size="sm" className="text-typography-500">
					{emptyMessage}
				</Text>
			</View>
		);
	}

	return (
		<ScrollView horizontal showsHorizontalScrollIndicator={false}>
			<View className="min-w-full">
				{/* Header */}
				<View className="flex-row py-2.5 px-4 bg-background-50 rounded-lg mb-1">
					{columns.map((column) => (
						<View
							key={String(column.key)}
							className="flex-1 min-w-[120px]"
							style={column.width ? { width: column.width } : undefined}
						>
							<Text
								size="xs"
								className="text-typography-500 font-semibold uppercase"
							>
								{column.title}
							</Text>
						</View>
					))}
				</View>

				{/* Rows */}
				{data.map((item) => (
					<Pressable
						key={keyExtractor(item)}
						onPress={() => onRowPress?.(item)}
						className={`flex-row py-2.5 px-4 rounded-lg mb-0.5 ${
							onRowPress
								? "hover:bg-background-50 active:opacity-70 cursor-pointer"
								: ""
						}`}
					>
						{columns.map((column) => (
							<View
								key={String(column.key)}
								className="flex-1 min-w-[120px] justify-center"
								style={column.width ? { width: column.width } : undefined}
							>
								{column.render ? (
									column.render(item)
								) : (
									<Text size="sm">
										{String(item[column.key as keyof T] ?? "")}
									</Text>
								)}
							</View>
						))}
					</Pressable>
				))}
			</View>
		</ScrollView>
	);
}
