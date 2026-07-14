"use client";

import { useState } from "react";
import {
	AlertDialog,
	AlertDialogBackdrop,
	AlertDialogBody,
	AlertDialogContent,
	AlertDialogFooter,
	AlertDialogHeader,
} from "@/components/ui/alert-dialog";
import { Button, ButtonSpinner, ButtonText } from "@/components/ui/button";
import { Heading } from "@/components/ui/heading";
import { Text } from "@/components/ui/text";

interface ConfirmDialogProps {
	open: boolean;
	onOpenChange: (open: boolean) => void;
	title: string;
	description: string;
	confirmText?: string;
	cancelText?: string;
	onConfirm: () => void | Promise<void>;
	destructive?: boolean;
}

export function ConfirmDialog({
	open,
	onOpenChange,
	title,
	description,
	confirmText = "Confirm",
	cancelText = "Cancel",
	onConfirm,
	destructive = false,
}: ConfirmDialogProps) {
	const [loading, setLoading] = useState(false);

	const handleConfirm = async () => {
		setLoading(true);
		try {
			await onConfirm();
			onOpenChange(false);
		} catch (error) {
			console.error("Confirm dialog error:", error);
		} finally {
			setLoading(false);
		}
	};

	return (
		<AlertDialog isOpen={open} onClose={() => onOpenChange(false)} size="md">
			<AlertDialogBackdrop />
			<AlertDialogContent>
				<AlertDialogHeader>
					<Heading size="md">{title}</Heading>
				</AlertDialogHeader>
				<AlertDialogBody>
					<Text size="sm" className="text-typography-500">
						{description}
					</Text>
				</AlertDialogBody>
				<AlertDialogFooter className="gap-3">
					<Button
						variant="outline"
						action="secondary"
						onPress={() => onOpenChange(false)}
						isDisabled={loading}
					>
						<ButtonText>{cancelText}</ButtonText>
					</Button>
					<Button
						action={destructive ? "negative" : "primary"}
						onPress={handleConfirm}
						isDisabled={loading}
					>
						{loading && <ButtonSpinner />}
						<ButtonText>{loading ? "..." : confirmText}</ButtonText>
					</Button>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
