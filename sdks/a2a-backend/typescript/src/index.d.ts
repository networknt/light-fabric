import type { Server } from "node:http";

export interface BusinessRequest { taskId: string; contextId: string; idempotencyKey: string; skillId: string | null; message: unknown; metadata: unknown }
export interface BusinessResponse { state: string; backendOperationId: string | null; result: unknown; error: unknown; artifacts: unknown[] }
export interface AgentBackend {
  capabilities(): object;
  invoke(context: object, request: BusinessRequest): Promise<BusinessResponse>;
  invokeStream(context: object, request: BusinessRequest): AsyncIterable<object>;
  status(context: object, request: BusinessRequest): Promise<BusinessResponse>;
  cancel(context: object, request: BusinessRequest): Promise<BusinessResponse>;
}
export interface AdapterConfig { key: Buffer; contractDigest: string; audience: string; hostId: string; environment: string; targetAgentRef: string; bindingId: string; publicationId: string; policyDigest: string; dataBoundaryDigest: string; replayFile: string; maximumRequestBytes?: number }
export function serve(backend: AgentBackend, config: AdapterConfig, port: number): Server;
export function verify(headers: object, body: Buffer, operation: string, config: AdapterConfig): object;
