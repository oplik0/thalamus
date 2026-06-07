"use client";

import { Link, useRouter } from "expo-router";
import { useState } from "react";
import { ScrollView } from "react-native";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertText } from "@/components/ui/alert";
import { Box } from "@/components/ui/box";
import { Button, ButtonSpinner, ButtonText } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
	FormControl,
	FormControlHelper,
	FormControlHelperText,
	FormControlLabel,
	FormControlLabelText,
} from "@/components/ui/form-control";
import { HStack } from "@/components/ui/hstack";
import { Input, InputField } from "@/components/ui/input";
import { Toast, ToastTitle, useToast } from "@/components/ui/toast";
import { useCreateTeam } from "@/hooks/use-teams";

export default function CreateTeamPage() {
	const router = useRouter();
	const createMutation = useCreateTeam();
	const toast = useToast();

	const [name, setName] = useState("");
	const [description, setDescription] = useState("");

	const handleCreate = async () => {
		if (!name.trim()) return;

		const team = await createMutation.mutateAsync({
			name: name.trim(),
			description: description.trim() || undefined,
		});

		toast.show({
			id: "team-created",
			render: () => (
				<Toast action="success">
					<ToastTitle>Team created</ToastTitle>
				</Toast>
			),
		});

		router.replace(`/(tabs)/(admin)/teams/${team.id}`);
	};

	return (
		<ScrollView className="flex-1 bg-background-0">
			<Box className="p-6 gap-6 max-w-2xl">
				<PageHeader
					title="Create Team"
					description="Create a new team to organize members and projects"
				/>

				<Card className="p-6 gap-5">
					<FormControl isRequired>
						<FormControlLabel>
							<FormControlLabelText>Name</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField
								value={name}
								onChangeText={setName}
								placeholder="e.g. Engineering"
							/>
						</Input>
					</FormControl>

					<FormControl>
						<FormControlLabel>
							<FormControlLabelText>Description</FormControlLabelText>
						</FormControlLabel>
						<Input>
							<InputField
								value={description}
								onChangeText={setDescription}
								placeholder="Optional description"
							/>
						</Input>
					</FormControl>

					<FormControl>
						<FormControlLabel>
							<FormControlLabelText>Slug</FormControlLabelText>
						</FormControlLabel>
						<FormControlHelper>
							<FormControlHelperText>
								The slug is generated automatically from the team name.
							</FormControlHelperText>
						</FormControlHelper>
					</FormControl>
				</Card>

				{createMutation.error && (
					<Alert action="error">
						<AlertText>
							{createMutation.error instanceof Error
								? createMutation.error.message
								: "Failed to create team"}
						</AlertText>
					</Alert>
				)}

				<HStack className="justify-end gap-3">
					<Link href="/(tabs)/(admin)/teams" asChild>
						<Button variant="outline" action="secondary">
							<ButtonText>Cancel</ButtonText>
						</Button>
					</Link>
					<Button
						onPress={handleCreate}
						isDisabled={!name.trim() || createMutation.isPending}
					>
						{createMutation.isPending && <ButtonSpinner />}
						<ButtonText>
							{createMutation.isPending ? "Creating..." : "Create Team"}
						</ButtonText>
					</Button>
				</HStack>
			</Box>
		</ScrollView>
	);
}
