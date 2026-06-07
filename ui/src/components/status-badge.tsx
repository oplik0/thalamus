"use client";

import { Badge, BadgeText } from "@/components/ui/badge";

type StatusVariant = "active" | "revoked" | "expired" | "inactive";

interface StatusBadgeProps {
	status: StatusVariant;
	label?: string;
}

const STATUS_MAP: Record<
	StatusVariant,
	{ action: "success" | "error" | "warning" | "muted"; text: string }
> = {
	active: { action: "success", text: "Active" },
	revoked: { action: "error", text: "Revoked" },
	expired: { action: "warning", text: "Expired" },
	inactive: { action: "muted", text: "Inactive" },
};

export function StatusBadge({ status, label }: StatusBadgeProps) {
	const config = STATUS_MAP[status] ?? STATUS_MAP.inactive;

	return (
		<Badge action={config.action} variant="outline" size="sm">
			<BadgeText>{label ?? config.text}</BadgeText>
		</Badge>
	);
}

/**
 * Derive status from API key/signing key fields
 */
export function deriveKeyStatus(key: {
	is_active: boolean;
	expires_at?: string | null;
}): StatusVariant {
	if (!key.is_active) return "revoked";
	if (key.expires_at && new Date(key.expires_at) < new Date()) return "expired";
	return "active";
}
