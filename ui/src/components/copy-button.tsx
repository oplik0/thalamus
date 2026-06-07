"use client";

import * as Clipboard from "expo-clipboard";
import { Check, Copy } from "lucide-react-native";
import { useCallback, useState } from "react";
import { Platform, Pressable } from "react-native";

interface CopyButtonProps {
	value: string;
	size?: number;
	className?: string;
}

export function CopyButton({ value, size = 18, className }: CopyButtonProps) {
	const [copied, setCopied] = useState(false);

	const handleCopy = useCallback(async () => {
		try {
			if (Platform.OS === "web") {
				await navigator.clipboard.writeText(value);
			} else {
				await Clipboard.setStringAsync(value);
			}
			setCopied(true);
			setTimeout(() => setCopied(false), 2000);
		} catch (error) {
			console.error("Failed to copy:", error);
		}
	}, [value]);

	return (
		<Pressable
			onPress={handleCopy}
			className={`p-1.5 rounded hover:bg-background-100 active:opacity-70 ${className ?? ""}`}
			accessibilityLabel="Copy to clipboard"
			accessibilityRole="button"
		>
			{copied ? (
				<Check size={size} color="#22c55e" />
			) : (
				<Copy size={size} className="text-typography-500" />
			)}
		</Pressable>
	);
}
