package com.networknt.a2a.backend;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;

import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.time.Instant;
import java.util.Base64;
import java.util.HexFormat;
import java.util.Map;
import java.util.Properties;
import java.util.HashSet;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.Executors;

public final class BackendAdapter {
    public static final String CONTRACT_VERSION = "light-a2a-backend/v1";
    private static final ObjectMapper MAPPER = new ObjectMapper();
    private static final String CONTEXT = "x-light-a2a-backend-context";
    private static final String SIGNATURE = "x-light-a2a-backend-signature";
    private static final String DIGEST = "x-light-a2a-backend-contract-digest";

    public record Config(byte[] key, String contractDigest, String audience, String hostId,
                         String environment, String targetAgentRef, String bindingId,
                         String publicationId, String policyDigest, String dataBoundaryDigest,
                         Path replayFile, int maximumRequestBytes) {
        public Config {
            if (key == null || key.length < 32 || maximumRequestBytes < 1) {
                throw new IllegalArgumentException("invalid adapter key or request limit");
            }
            key = key.clone();
        }
    }

    private BackendAdapter() { }

    public static HttpServer serve(AgentBackend backend, Config config, int port) throws IOException {
        HttpServer server = HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), port), 32);
        server.setExecutor(Executors.newVirtualThreadPerTaskExecutor());
        server.createContext("/health/live", exchange -> empty(exchange, 204));
        server.createContext("/health/ready", exchange -> empty(exchange, 204));
        server.createContext("/v1/capabilities", exchange -> {
            if (!"GET".equals(exchange.getRequestMethod())) { empty(exchange, 405); return; }
            if (!config.contractDigest().equals(first(exchange, DIGEST))) {
                error(exchange, 401, new SecurityException("contract digest mismatch")); return;
            }
            json(exchange, 200, backend.capabilities());
        });
        Map.of("/v1/invoke", "INVOKE", "/v1/invoke-stream", "INVOKE_STREAM",
                "/v1/status", "STATUS", "/v1/cancel", "CANCEL")
                .forEach((path, operation) -> server.createContext(path,
                        exchange -> dispatch(exchange, backend, config, operation)));
        server.start();
        return server;
    }

    public static JsonNode verify(Map<String, String> headers, byte[] body, String operation,
                                  Config config) throws Exception {
        String encoded = headers.get(CONTEXT);
        String signature = headers.get(SIGNATURE);
        if (encoded == null || signature == null || !config.contractDigest().equals(headers.get(DIGEST))) {
            throw new SecurityException("missing or mismatched signed invocation headers");
        }
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec(config.key(), "HmacSHA256"));
        mac.update(encoded.getBytes(StandardCharsets.US_ASCII)); mac.update((byte) 0); mac.update(body);
        byte[] supplied;
        try {
            supplied = HexFormat.of().parseHex(signature);
        } catch (IllegalArgumentException error) {
            throw new SecurityException("malformed signature", error);
        }
        if (!MessageDigest.isEqual(mac.doFinal(), supplied)) {
            throw new SecurityException("signature mismatch");
        }
        JsonNode context = MAPPER.readTree(Base64.getUrlDecoder().decode(pad(encoded)));
        JsonNode request = MAPPER.readTree(body);
        Set<String> contextFields=Set.of("contractVersion","invocationId","issuer","audience","hostId","environment","principalSubject","callerAgentRef","targetAgentRef","bindingId","publicationId","selectedSkillId","operation","taskId","contextId","idempotencyKey","backendOperationId","policyDigest","dataBoundaryDigest","requestDigest","budget","traceparent","issuedAt","deadline","expiresAt");
        Set<String> requestFields=Set.of("taskId","contextId","idempotencyKey","skillId","message","metadata");
        Set<String> actualContext=new HashSet<>();context.fieldNames().forEachRemaining(actualContext::add);
        Set<String> actualRequest=new HashSet<>();request.fieldNames().forEachRemaining(actualRequest::add);
        if(!actualContext.equals(contextFields)||!actualRequest.equals(requestFields)) throw new SecurityException("unknown or missing contract field");
        for(String field:new String[]{"invocationId","hostId","bindingId","publicationId","taskId","contextId"}) UUID.fromString(context.path(field).asText());
        Map<String, String> fixed = Map.ofEntries(
                Map.entry("contractVersion", CONTRACT_VERSION), Map.entry("issuer", "light-a2a"),
                Map.entry("audience", config.audience()), Map.entry("hostId", config.hostId()),
                Map.entry("environment", config.environment()), Map.entry("targetAgentRef", config.targetAgentRef()),
                Map.entry("bindingId", config.bindingId()), Map.entry("publicationId", config.publicationId()),
                Map.entry("policyDigest", config.policyDigest()), Map.entry("dataBoundaryDigest", config.dataBoundaryDigest()),
                Map.entry("operation", operation));
        for (var entry : fixed.entrySet()) if (!entry.getValue().equals(context.path(entry.getKey()).asText())) throw new SecurityException("invocation " + entry.getKey() + " mismatch");
        Instant now=Instant.now(),issued=Instant.parse(context.path("issuedAt").asText()),deadline=Instant.parse(context.path("deadline").asText()),expires=Instant.parse(context.path("expiresAt").asText());
        JsonNode budget=context.path("budget");
        if(issued.isAfter(now.plusSeconds(30))||!deadline.isAfter(now)||!expires.isAfter(now)||expires.isAfter(deadline)||expires.isAfter(issued.plusSeconds(300))
                || java.util.List.of("principalSubject","callerAgentRef","targetAgentRef","environment","idempotencyKey").stream().anyMatch(field->context.path(field).asText().isBlank())
                || java.util.List.of("policyDigest","dataBoundaryDigest","requestDigest").stream().anyMatch(field->!context.path(field).asText().matches("^sha256:[0-9a-f]{64}$"))
                || java.util.List.of("maximumInputBytes","maximumOutputBytes","maximumArtifactBytes").stream().anyMatch(field->!budget.path(field).canConvertToLong()||budget.path(field).asLong()<1)) throw new SecurityException("invalid invocation envelope");
        String bodyDigest = "sha256:" + HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(body));
        if (!bodyDigest.equals(context.path("requestDigest").asText())) throw new SecurityException("request digest mismatch");
        for (String field : new String[]{"taskId", "contextId", "idempotencyKey"}) if (!request.path(field).equals(context.path(field))) throw new SecurityException("unsigned " + field);
        if (!request.path("skillId").equals(context.path("selectedSkillId"))) throw new SecurityException("unsigned skillId");
        if ((operation.equals("STATUS") || operation.equals("CANCEL")) && context.path("backendOperationId").asText().isBlank()) throw new SecurityException("missing backend operation identity");
        if (!consumeReplay(config.replayFile(), context.path("invocationId").asText(), expires)) throw new SecurityException("replayed invocation");
        return context;
    }

    private static void dispatch(HttpExchange exchange, AgentBackend backend, Config config, String operation) throws IOException {
        try {
            if (!"POST".equals(exchange.getRequestMethod())) { empty(exchange, 405); return; }
            byte[] body = exchange.getRequestBody().readNBytes(config.maximumRequestBytes() + 1);
            if (body.length > config.maximumRequestBytes()) throw new IllegalArgumentException("request exceeded limit");
            Map<String, String> headers = Map.of(CONTEXT, first(exchange, CONTEXT), SIGNATURE, first(exchange, SIGNATURE), DIGEST, first(exchange, DIGEST));
            JsonNode context = verify(headers, body, operation, config); JsonNode request = MAPPER.readTree(body);
            if (operation.equals("INVOKE_STREAM")) {
                exchange.getResponseHeaders().set("content-type", "text/event-stream"); exchange.getResponseHeaders().set("cache-control", "no-store"); exchange.sendResponseHeaders(200, 0);
                long previous = 0; boolean terminal = false;
                for (JsonNode event : backend.invokeStream(context, request)) { long sequence = event.path("sequenceNumber").asLong(); if (sequence <= previous) throw new IllegalStateException("events are not ordered"); previous = sequence; terminal = event.path("terminal").asBoolean(); exchange.getResponseBody().write(("data: " + MAPPER.writeValueAsString(event) + "\n\n").getBytes(StandardCharsets.UTF_8)); exchange.getResponseBody().flush(); }
                if (!terminal) throw new IllegalStateException("stream did not terminate"); exchange.close(); return;
            }
            JsonNode response = switch (operation) { case "INVOKE" -> backend.invoke(context, request); case "STATUS" -> backend.status(context, request); case "CANCEL" -> backend.cancel(context, request); default -> throw new IllegalStateException(); };
            json(exchange, 200, response);
        } catch (SecurityException error) { error(exchange, 401, error); }
        catch (Exception error) { error(exchange, 422, error); }
    }

    private static synchronized boolean consumeReplay(Path path, String id, Instant expires) throws IOException {
        Properties values = new Properties(); if (Files.exists(path)) try (var input = Files.newInputStream(path)) { values.load(input); }
        long now = Instant.now().toEpochMilli(); values.entrySet().removeIf(entry -> Long.parseLong(entry.getValue().toString()) <= now);
        if (values.containsKey(id)) return false; values.setProperty(id, Long.toString(expires.toEpochMilli()));
        Path temporary = path.resolveSibling(path.getFileName() + ".tmp"); try (var output = Files.newOutputStream(temporary)) { values.store(output, "light-a2a replay state"); }
        Files.move(temporary, path, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE); return true;
    }

    private static String pad(String value) { return value + "=".repeat((4 - value.length() % 4) % 4); }
    private static String first(HttpExchange exchange, String name) { String value = exchange.getRequestHeaders().getFirst(name); return value == null ? "" : value; }
    private static void empty(HttpExchange exchange, int status) throws IOException { exchange.sendResponseHeaders(status, -1); exchange.close(); }
    private static void json(HttpExchange exchange, int status, JsonNode value) throws IOException { byte[] body = MAPPER.writeValueAsBytes(value); exchange.getResponseHeaders().set("content-type", "application/json"); exchange.sendResponseHeaders(status, body.length); exchange.getResponseBody().write(body); exchange.close(); }
    private static void error(HttpExchange exchange, int status, Exception error) throws IOException { json(exchange, status, MAPPER.createObjectNode().put("code", "BACKEND_INVOCATION_REJECTED").put("message", error.getMessage()).put("retryable", false)); }
}
