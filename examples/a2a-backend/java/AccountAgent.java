import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import com.networknt.a2a.backend.AgentBackend;

public final class AccountAgent implements AgentBackend {
    public JsonNode capabilities() { var value=JsonNodeFactory.instance.objectNode().put("contractVersion", "light-a2a-backend/v1").put("streaming", false).put("cancellation", true).put("statusReconciliation", true).put("maximumArtifactBytes", 1048576); value.putArray("acceptedContentModes").add("text/plain"); return value; }
    public JsonNode invoke(JsonNode context, JsonNode request) { return JsonNodeFactory.instance.objectNode().put("state", "COMPLETED").put("backendOperationId", "account:" + request.path("taskId").asText()).set("result", JsonNodeFactory.instance.objectNode().put("answer", "business result")); }
    public Iterable<JsonNode> invokeStream(JsonNode context, JsonNode request) { return java.util.List.of(); }
    public JsonNode status(JsonNode context, JsonNode request) { return invoke(context, request); }
    public JsonNode cancel(JsonNode context, JsonNode request) { return JsonNodeFactory.instance.objectNode().put("state", "CANCELED").put("backendOperationId", "account:" + request.path("taskId").asText()); }
}
