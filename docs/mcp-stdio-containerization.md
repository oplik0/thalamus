# Containerized MCP stdio plan

Runtime stdio registration is disabled. Re-enable it only through a process-runner boundary that does not execute commands in the Thalamus container.

## Required boundary

- Replace direct `tokio::process::Command` use with an `McpProcessRunner` interface.
- Implement the production runner as an isolated container or Kubernetes Job/Pod, not a child process.
- Select images from a platform-admin allowlist. Tenant input must not supply arbitrary images, entrypoints, mounts, capabilities, or runtime flags.
- Pass credentials through short-lived secret references. Do not copy the Thalamus process environment into the workload.
- Disable host networking and use the same outbound destination policy as HTTP MCP connections.
- Use a read-only root filesystem, non-root UID, dropped Linux capabilities, seccomp, and no host mounts.
- Enforce CPU, memory, process, output, startup, idle, and wall-clock limits.
- Bind each workload to tenant and principal identity and include that identity in audit events and session keys.
- Terminate the workload on session eviction, credential rotation, server disable/delete, or application shutdown.

## Delivery order

1. Define `McpProcessRunner` and lifecycle tests using a fake runner.
2. Add a container runtime implementation behind platform configuration.
3. Add image policy, resource limits, network policy, and secret injection.
4. Add health, cancellation, cleanup, and orphan reconciliation.
5. Permit platform administrators to register approved stdio templates.
6. Consider tenant registration only after an explicit template/parameter policy exists.
