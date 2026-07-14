import {
	TabList,
	type TabListProps,
	TabSlot,
	Tabs,
	TabTrigger,
	type TabTriggerSlotProps,
} from "expo-router/ui";
import { Pressable, View } from "react-native";
import { Text } from "@/components/ui/text";

export default function AppTabs() {
	return (
		<Tabs>
			<TabSlot style={{ height: "100%" }} />
			<TabList asChild>
				<CustomTabList>
					<TabTrigger name="home" href="/" asChild>
						<TabButton>Home</TabButton>
					</TabTrigger>
				</CustomTabList>
			</TabList>
		</Tabs>
	);
}

export function TabButton({
	children,
	isFocused,
	...props
}: TabTriggerSlotProps) {
	return (
		<Pressable {...props} className="active:opacity-70">
			<View
				className={`py-1 px-4 rounded-xl ${
					isFocused ? "bg-background-200" : "bg-background-50"
				}`}
			>
				<Text
					size="sm"
					className={isFocused ? "text-typography-900" : "text-typography-500"}
				>
					{children}
				</Text>
			</View>
		</Pressable>
	);
}

export function CustomTabList(props: TabListProps) {
	return (
		<View
			{...props}
			className="absolute w-full p-4 justify-center items-center flex-row"
		>
			<View className="py-2 px-8 rounded-3xl flex-row items-center flex-grow gap-2 max-w-3xl bg-background-50">
				<Text className="mr-auto font-semibold text-sm">Thalamus</Text>
				{props.children}
			</View>
		</View>
	);
}
