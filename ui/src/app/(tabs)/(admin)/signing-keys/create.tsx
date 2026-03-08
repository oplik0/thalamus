"use client";

import { useState } from "react";
import { Link, useRouter } from "expo-router";
import { ScrollView, View } from "react-native";
import { Card } from "@/components/ui/card";
import { Text } from "@/components/ui/text";
import { Heading } from "@/components/ui/heading";
import { Input, InputField } from "@/components/ui/input";
import { Button, ButtonText, ButtonSpinner } from "@/components/ui/button";
import {
  FormControl,
  FormControlLabel,
  FormControlLabelText,
  FormControlHelper,
  FormControlHelperText,
} from "@/components/ui/form-control";
import { HStack } from "@/components/ui/hstack";
import { VStack } from "@/components/ui/vstack";
import { Box } from "@/components/ui/box";
import { Divider } from "@/components/ui/divider";
import { Alert, AlertText } from "@/components/ui/alert";
import { PageHeader } from "@/components/page-header";
import { CopyButton } from "@/components/copy-button";
import { useCreateSigningKey } from "@/hooks/use-signing-keys";
import { CreateSigningKeyResponse, SigningAlgorithm } from "@/lib/types";
import { useToast, Toast, ToastTitle } from "@/components/ui/toast";
import { AlertTriangle, ShieldCheck } from "lucide-react-native";

const ALGORITHMS: { value: SigningAlgorithm; label: string; desc: string }[] = [
  { value: "Ed25519", label: "Ed25519", desc: "EdDSA - fast, small keys" },
  { value: "ES256", label: "ES256", desc: "ECDSA P-256" },
  { value: "ES384", label: "ES384", desc: "ECDSA P-384" },
  { value: "RS256", label: "RS256", desc: "RSA 2048-bit" },
  { value: "RS384", label: "RS384", desc: "RSA 3072-bit" },
  { value: "RS512", label: "RS512", desc: "RSA 4096-bit" },
];

const EXPIRY_OPTIONS = [
  { label: "Never", value: undefined },
  { label: "90 days", value: 90 },
  { label: "1 year", value: 365 },
  { label: "2 years", value: 730 },
];

export default function CreateSigningKeyPage() {
  const router = useRouter();
  const createMutation = useCreateSigningKey();
  const toast = useToast();

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [algorithm, setAlgorithm] = useState<SigningAlgorithm>("Ed25519");
  const [expiresInDays, setExpiresInDays] = useState<number | undefined>(
    undefined,
  );
  const [createdKey, setCreatedKey] =
    useState<CreateSigningKeyResponse | null>(null);

  const handleCreate = async () => {
    const result = await createMutation.mutateAsync({
      algorithm,
      name: name.trim() || undefined,
      description: description.trim() || undefined,
      expires_in_days: expiresInDays,
    });

    setCreatedKey(result);
    toast.show({
      id: "signing-key-created",
      render: () => (
        <Toast action="success">
          <ToastTitle>Signing key created</ToastTitle>
        </Toast>
      ),
    });
  };

  // Key created - show keys
  if (createdKey) {
    return (
      <ScrollView className="flex-1 bg-background-0">
        <Box className="p-6 gap-6 max-w-2xl">
          <PageHeader title="Signing Key Created" />

          <Card className="p-6 gap-4">
            <HStack className="items-center gap-2">
              <ShieldCheck size={28} className="text-success-500" />
              <Heading size="md">Save the private key securely</Heading>
            </HStack>

            <Alert action="warning">
              <AlertTriangle size={16} className="text-warning-600" />
              <AlertText>{createdKey.warning}</AlertText>
            </Alert>

            <VStack className="gap-2">
              <HStack className="justify-between items-center">
                <Text size="sm" className="font-semibold">
                  Private Key
                </Text>
                <CopyButton value={createdKey.private_key} />
              </HStack>
              <Box className="bg-background-50 p-3 rounded-lg">
                <Text
                  size="xs"
                  className="font-mono break-all"
                  selectable
                >
                  {createdKey.private_key}
                </Text>
              </Box>
            </VStack>

            <Divider />

            <VStack className="gap-2">
              <HStack className="justify-between items-center">
                <Text size="sm" className="font-semibold">
                  Public Key
                </Text>
                <CopyButton value={createdKey.public_key} />
              </HStack>
              <Box className="bg-background-50 p-3 rounded-lg">
                <Text
                  size="xs"
                  className="font-mono break-all"
                  selectable
                >
                  {createdKey.public_key}
                </Text>
              </Box>
            </VStack>

            <VStack className="gap-1">
              <Text size="xs" className="text-typography-500">
                Algorithm: {createdKey.algorithm}
              </Text>
              <Text size="xs" className="text-typography-500">
                Fingerprint: {createdKey.fingerprint}
              </Text>
              <Text size="xs" className="text-typography-500">
                Key ID: {createdKey.key_id}
              </Text>
            </VStack>
          </Card>

          <HStack className="justify-end">
            <Link href="/(tabs)/(admin)/signing-keys" asChild>
              <Button>
                <ButtonText>Done</ButtonText>
              </Button>
            </Link>
          </HStack>
        </Box>
      </ScrollView>
    );
  }

  return (
    <ScrollView className="flex-1 bg-background-0">
      <Box className="p-6 gap-6 max-w-2xl">
        <PageHeader
          title="Create Signing Key"
          description="Generate a new key pair for HTTP signature authentication"
        />

        <Card className="p-6 gap-5">
          <FormControl>
            <FormControlLabel>
              <FormControlLabelText>Algorithm</FormControlLabelText>
            </FormControlLabel>
            <View className="flex-row flex-wrap gap-2 mt-1">
              {ALGORITHMS.map((alg) => (
                <Button
                  key={alg.value}
                  size="sm"
                  variant={algorithm === alg.value ? "solid" : "outline"}
                  action={algorithm === alg.value ? "primary" : "secondary"}
                  onPress={() => setAlgorithm(alg.value)}
                >
                  <ButtonText>{alg.label}</ButtonText>
                </Button>
              ))}
            </View>
            <FormControlHelper>
              <FormControlHelperText>
                {ALGORITHMS.find((a) => a.value === algorithm)?.desc}
              </FormControlHelperText>
            </FormControlHelper>
          </FormControl>

          <FormControl>
            <FormControlLabel>
              <FormControlLabelText>Name (optional)</FormControlLabelText>
            </FormControlLabel>
            <Input>
              <InputField
                value={name}
                onChangeText={setName}
                placeholder="e.g. Production Signing Key"
              />
            </Input>
          </FormControl>

          <FormControl>
            <FormControlLabel>
              <FormControlLabelText>
                Description (optional)
              </FormControlLabelText>
            </FormControlLabel>
            <Input>
              <InputField
                value={description}
                onChangeText={setDescription}
                placeholder="Optional description"
              />
            </Input>
          </FormControl>

          <Divider />

          <FormControl>
            <FormControlLabel>
              <FormControlLabelText>Expiration</FormControlLabelText>
            </FormControlLabel>
            <HStack className="gap-2 flex-wrap">
              {EXPIRY_OPTIONS.map((option) => (
                <Button
                  key={option.label}
                  size="sm"
                  variant={
                    expiresInDays === option.value ? "solid" : "outline"
                  }
                  action={
                    expiresInDays === option.value ? "primary" : "secondary"
                  }
                  onPress={() => setExpiresInDays(option.value)}
                >
                  <ButtonText>{option.label}</ButtonText>
                </Button>
              ))}
            </HStack>
          </FormControl>
        </Card>

        {createMutation.error && (
          <Alert action="error">
            <AlertText>
              {createMutation.error instanceof Error
                ? createMutation.error.message
                : "Failed to create signing key"}
            </AlertText>
          </Alert>
        )}

        <HStack className="justify-end gap-3">
          <Link href="/(tabs)/(admin)/signing-keys" asChild>
            <Button variant="outline" action="secondary">
              <ButtonText>Cancel</ButtonText>
            </Button>
          </Link>
          <Button
            onPress={handleCreate}
            isDisabled={createMutation.isPending}
          >
            {createMutation.isPending && <ButtonSpinner />}
            <ButtonText>
              {createMutation.isPending ? "Creating..." : "Create Key"}
            </ButtonText>
          </Button>
        </HStack>
      </Box>
    </ScrollView>
  );
}
