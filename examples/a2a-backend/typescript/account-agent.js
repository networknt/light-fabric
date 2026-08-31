export const accountAgent = {
  capabilities: () => ({ contractVersion: "light-a2a-backend/v1", streaming: false, cancellation: true, statusReconciliation: true, acceptedContentModes: ["text/plain"], maximumArtifactBytes: 1_048_576 }),
  invoke: async (_context, request) => ({ state: "COMPLETED", backendOperationId: `account:${request.taskId}`, result: { answer: "business result" }, error: null, artifacts: [] }),
  invokeStream: async function* () {},
  status: async (context, request) => accountAgent.invoke(context, request),
  cancel: async (_context, request) => ({ state: "CANCELED", backendOperationId: `account:${request.taskId}`, result: null, error: null, artifacts: [] }),
};

// Deployment calls the SDK serve() function with generated adapter config.
// This file intentionally contains business callbacks only.
