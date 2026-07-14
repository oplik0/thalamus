"use client";

import { Link, Slot, usePathname } from "expo-router";
import {
	FileKey2,
	KeyRound,
	LayoutDashboard,
	Settings,
	Shield,
	Users,
} from "lucide-react-native";
import { Platform, Pressable, View } from "react-native";
import { AuthGuard } from "@/components/auth-guard";
import { Divider } from "@/components/ui/divider";
import { Heading } from "@/components/ui/heading";
import { Text } from "@/components/ui/text";

const NAV_ITEMS = [
	{
		name: "Dashboard",
		href: "/(tabs)/(admin)",
		icon: LayoutDashboard,
	},
	{
		name: "Users",
		href: "/(tabs)/(admin)/users",
		icon: Users,
	},
	{
		name: "Teams",
		href: "/(tabs)/(admin)/teams",
		icon: Users,
	},
	{
		name: "API Keys",
		href: "/(tabs)/(admin)/api-keys",
		icon: KeyRound,
	},
	{
		name: "Signing Keys",
		href: "/(tabs)/(admin)/signing-keys",
		icon: FileKey2,
	},
	{
		name: "Authorization",
		href: "/(tabs)/(admin)/authorization",
		icon: Shield,
	},
	{
		name: "Settings",
		href: "/(tabs)/(admin)/settings",
		icon: Settings,
	},
] as const;

export default function AdminLayout() {
	const pathname = usePathname();
	const isWeb = Platform.OS === "web";

	return (
		<AuthGuard>
			<View className="flex-1 flex-row bg-background-0">
				{/* Sidebar */}
				<View
					className={`bg-background-50 py-4 gap-4 ${
						isWeb ? "w-60 px-4 border-r border-outline-200" : "w-20"
					}`}
				>
					<View className="px-3 py-3 gap-0.5">
						<Heading size="lg">Thalamus</Heading>
						<Text size="xs" className="text-typography-500">
							Admin Panel
						</Text>
					</View>

					<Divider />

					<View className="gap-1">
						{NAV_ITEMS.map((item) => {
							const isActive =
								pathname === item.href ||
								(item.href === "/(tabs)/(admin)" &&
									pathname === "/(tabs)/(admin)/") ||
								(item.href !== "/(tabs)/(admin)" &&
									pathname.startsWith(item.href));

							const Icon = item.icon;

							return (
								<Link key={item.name} href={item.href} asChild>
									<Pressable
										accessibilityRole="link"
										accessibilityLabel={`Navigate to ${item.name}`}
										accessibilityState={{ selected: isActive }}
										className={`flex-row items-center gap-3 py-2.5 px-3 rounded-lg ${
											isActive
												? "bg-primary-50 dark:bg-primary-950"
												: "hover:bg-background-100 active:opacity-70"
										}`}
									>
										<Icon
											size={20}
											className={
												isActive ? "text-primary-600" : "text-typography-500"
											}
										/>
										{isWeb && (
											<Text
												size="sm"
												className={`flex-1 ${
													isActive
														? "text-primary-600 font-semibold"
														: "text-typography-700"
												}`}
											>
												{item.name}
											</Text>
										)}
									</Pressable>
								</Link>
							);
						})}
					</View>
				</View>

				{/* Main content area */}
				<View className="flex-1">
					<Slot />
				</View>
			</View>
		</AuthGuard>
	);
}
