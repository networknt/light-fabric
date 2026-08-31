package com.networknt.a2a.backend;

import com.fasterxml.jackson.databind.JsonNode;

public interface AgentBackend {
    JsonNode capabilities();
    JsonNode invoke(JsonNode context, JsonNode request) throws Exception;
    Iterable<JsonNode> invokeStream(JsonNode context, JsonNode request) throws Exception;
    JsonNode status(JsonNode context, JsonNode request) throws Exception;
    JsonNode cancel(JsonNode context, JsonNode request) throws Exception;
}
