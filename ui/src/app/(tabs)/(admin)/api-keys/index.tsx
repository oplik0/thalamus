"use client";

import { useState } from "react";
import { Link } from "expo-router";
import { ScrollView, View } from "react-native";
import { Card } from "@/components/ui/card";
import { Text } from "@/components/ui/text";
import { Button, ButtonText, ButtonIcon } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { HStack } from "@/components/ui/hstack";
import { VStack } from "@/components/ui/vstack";
import { Box } from "@/components/ui/box";
import { Badge, BadgeText } from "@/components/ui/badge";
import { PageHeader } from "@/components/page-header";
import { EmptyState } from "@/components/empty-state";
import { StatusBadge, deriveKeyStatus } from "@/components/status-badge";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { useApiKeys, useRevokeApiKey, useRotateApiKey } from "@/hooks/use-api-keys";
import { ApiKeyInfo } from "@/lib/types";
import {
  Plus,
  KeyRound,
  RotateCw,
  Ban,
  Clock,
} from "lucide-react-native";
import { useToast, Toast, ToastTitle } from "@/components/ui/toast";

function ApiKeyRow({
  apiKey,
  onRevoke,
  onRotate,
}: {
  apiKey: ApiKeyInfo;
  onRevoke: () => void;
  onRotate: () => void;
}) {
  const status = deriveKeyStatus(apiKey);

  return (
    <Card className="p-4">
      <HStack className="justify-between items-start">
        <VStack className="flex-1 gap-1.5">
          <HStack className="items-center gap-2">
            <Text className="font-semibold">{apiKey.name}</Text>
            <StatusBadge status={status} />
          </HStack>

          <Text size="xs" className="text-typography-500 font-mono">
            {apiKey.key_prefix}...
          </Text>

          {apiKey.description && (
            <Text size="xs" className="text-typography-500">
              {apiKey.description}
            </Text>
          )}

          <HStack className="gap-3 mt-1 flex-wrap">
            {apiKey.scopes?.map((scope) => (
              <Badge key={scope} action="info" size="sm">
                <BadgeText>{scope}</BadgeText>
              </Badge>
            ))}
          </HStack>

          <HStack className="gap-4 mt-1">
            <Text size="xs" className="text-typography-400">
              Created {new Date(apiKey.created_at).toLocaleDateString()}
            </Text>
            {apiKey.expires_at && (
              <HStack className="items-center gap-1">
                <Clock size={12} className="text-typography-400" />
                <Text size="xs" className="text-typography-400">
                  Expires{" "}
                  {new Date(apiKey.expires_at).toLocaleDateString()}
                </Text>
              </HStack>
            )}
          </HStack>
        </VStack>

        {apiKey.is_active && (
          <HStack className="gap-1">
            <Button
              size="xs"
              variant="outline"
              action="secondary"
              onPress={onRotate}
              accessibilityLabel="Rotate API key"
            >
              <ButtonIcon as={RotateCw} />
            </Button>
            <Button
              size="xs"
              variant="outline"
              action="negative"
              onPress={onRevoke}
              accessibilityLabel="Revoke API key"
            >
              <ButtonIcon as={Ban} />
            </Button>
          </HStack>
        )}
      </HStack>
    </Card>
  );
}

export default function ApiKeysPage() {
  const { data: apiKeys, isLoading } = useApiKeys();
  const revokeMutation = useRevokeApiKey();
  const rotateMutation = useRotateApiKey();
  const toast = useToast();

  const [revokeTarget, setRevokeTarget] = useState<ApiKeyInfo | null>(null);
  const [rotateTarget, setRotateTarget] = useState<ApiKeyInfo | null>(null);

  const handleRevoke = async () => {
    if (!revokeTarget) return;
    await revokeMutation.mutateAsync(revokeTarget.id);
    toast.show({
      id: `revoke-${revokeTarget.id}`,
      render: () => (
        <Toast action="success">
          <ToastTitle>API key revoked</ToastTitle>
        </Toast>
      ),
    });
  };

  const handleRotate = async () => {
    if (!rotateTarget) return;
    await rotateMutation.mutateAsync({ keyId: rotateTarget.id });
    toast.show({
      id: `rotate-${rotateTarget.id}`,
      render: () => (
        <Toast action="success">
          <ToastTitle>API key rotated</ToastTitle>
        </Toast>
      ),
    });
  };

  return (
    <ScrollView className="flex-1 bg-background-0">
      <Box className="p-6 gap-6 max-w-5xl">
        <PageHeader
          title="API Keys"
          description="Manage API keys for authenticating with the Thalamus API"
          actions={
            <Link href="/(tabs)/(admin)/api-keys/create" asChild>
              <Button size="sm">
                <ButtonIcon as={Plus} />
                <ButtonText>Create Key</ButtonText>
              </Button>
            </Link>
          }
        />

        {isLoading ? (
          <Box className="py-12 items-center">
            <Spinner size="large" />
          </Box>
        ) : !apiKeys || apiKeys.length === 0 ? (
          <EmptyState
            icon={<KeyRound size={32} className="text-typography-400" />}
            title="No API keys yet"
            description="Create your first API key to start authenticating with the Thalamus API"
          />
        ) : (
          <VStack className="gap-3">
            {apiKeys.map((apiKey) => (
              <ApiKeyRow
                key={apiKey.id}
                apiKey={apiKey}
                onRevoke={() => setRevokeTarget(apiKey)}
                onRotate={() => setRotateTarget(apiKey)}
              />
            ))}
          </VStack>
        )}
      </Box>

      <ConfirmDialog
        open={!!revokeTarget}
        onOpenChange={(open) => !open && setRevokeTarget(null)}
        title="Revoke API Key"
        description={`Are you sure you want to revoke "${revokeTarget?.name}"? This action cannot be undone and will immediately invalidate all requests using this key.`}
        confirmText="Revoke"
        onConfirm={handleRevoke}
        destructive
      />

      <ConfirmDialog
        open={!!rotateTarget}
        onOpenChange={(open) => !open && setRotateTarget(null)}
        title="Rotate API Key"
        description={`Rotate "${rotateTarget?.name}"? A new key will be generated and the old one will remain active for a grace period.`}
        confirmText="Rotate"
        onConfirm={handleRotate}
      />
    </ScrollView>
  );
}
