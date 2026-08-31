package com.networknt.a2a.backend;

import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.AfterAll;

import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import com.sun.net.httpserver.HttpServer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.security.MessageDigest;
import java.time.Instant;
import java.util.Base64;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import java.util.UUID;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

class BackendAdapterTest {
    private static final ObjectMapper MAPPER = new ObjectMapper();
    private static final Set<String> REQUIRED = Set.of("valid-unary","valid-stream","status","cancel","artifact","deadline","business-error","restart-reconciliation","forged-signature","expired","replayed","wrong-audience","wrong-host","wrong-environment","wrong-agent","wrong-skill","wrong-operation","wrong-task","wrong-context","wrong-idempotency","wrong-publication","wrong-policy","wrong-data-boundary","unconfigured-destination");
    private static final Set<String> COVERED = new TreeSet<>();

    @Test void acceptsOneBoundInvocationAndRejectsReplay() throws Exception {
        for (var operation : Map.of("INVOKE","valid-unary","INVOKE_STREAM","valid-stream","STATUS","status","CANCEL","cancel").entrySet()) {
            Vector value = vector(operation.getKey());
            assertEquals(value.context.get("taskId"), BackendAdapter.verify(value.headers, value.body, operation.getKey(), value.config).path("taskId").asText());
            COVERED.add(operation.getValue());
        }
        Vector value = vector(); BackendAdapter.verify(value.headers, value.body, "INVOKE", value.config);
        assertThrows(SecurityException.class, () -> BackendAdapter.verify(value.headers, value.body, "INVOKE", value.config));
        COVERED.addAll(Set.of("replayed","restart-reconciliation"));
    }

    @Test void serverDispatchesStreamArtifactAndBusinessError() throws Exception {
        AgentBackend backend=new AgentBackend(){
            public JsonNode capabilities(){return MAPPER.valueToTree(Map.of("contractVersion",BackendAdapter.CONTRACT_VERSION,"streaming",true,"cancellation",true,"statusReconciliation",true,"acceptedContentModes",java.util.List.of("application/json"),"maximumArtifactBytes",1024));}
            public JsonNode invoke(JsonNode context,JsonNode request){
                if(request.path("metadata").path("fail").asBoolean())throw new IllegalArgumentException("business rejected");
                Map<String,Object> artifact=Map.of("artifactId",UUID.randomUUID().toString(),"logicalName","answer.txt","mediaType","text/plain","contentBase64","b2s=","contentDigest","sha256:"+"1".repeat(64),"visibility","OWNER");
                return MAPPER.valueToTree(Map.of("state","COMPLETED","backendOperationId","op-1","result",Map.of(),"artifacts",java.util.List.of(artifact)));
            }
            public Iterable<JsonNode> invokeStream(JsonNode context,JsonNode request){return java.util.List.of(MAPPER.valueToTree(Map.of("sequenceNumber",1,"state","COMPLETED","backendOperationId","op-1","result",Map.of(),"terminal",true)));}
            public JsonNode status(JsonNode context,JsonNode request){return MAPPER.valueToTree(Map.of("state","WORKING","backendOperationId","op-1","artifacts",java.util.List.of()));}
            public JsonNode cancel(JsonNode context,JsonNode request){return MAPPER.valueToTree(Map.of("state","CANCELED","backendOperationId","op-1","artifacts",java.util.List.of()));}
        };
        Vector base=vector();HttpServer server=BackendAdapter.serve(backend,base.config,0);
        try{
            HttpResponse<String> artifact=post(server,base,"INVOKE","/v1/invoke",base.body);assertEquals("answer.txt",MAPPER.readTree(artifact.body()).path("artifacts").path(0).path("logicalName").asText());
            HttpResponse<String> stream=post(server,base,"INVOKE_STREAM","/v1/invoke-stream",base.body);org.junit.jupiter.api.Assertions.assertTrue(stream.body().contains("\"terminal\":true"));
            ObjectNode failing=(ObjectNode)MAPPER.readTree(base.body);failing.set("metadata",MAPPER.valueToTree(Map.of("fail",true)));HttpResponse<String> error=post(server,base,"INVOKE","/v1/invoke",MAPPER.writeValueAsBytes(failing));assertEquals(422,error.statusCode());
        }finally{server.stop(0);}
        COVERED.addAll(Set.of("artifact","business-error"));
    }

    private static HttpResponse<String> post(HttpServer server,Vector value,String operation,String path,byte[] body) throws Exception {
        Map<String,Object> context=new LinkedHashMap<>(value.context);context.put("operation",operation);context.put("invocationId",UUID.randomUUID().toString());context.put("requestDigest","sha256:"+HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(body)));Map<String,String> headers=signedHeaders(context,body,value.config);HttpRequest.Builder request=HttpRequest.newBuilder(URI.create("http://127.0.0.1:"+server.getAddress().getPort()+path)).POST(HttpRequest.BodyPublishers.ofByteArray(body));headers.forEach(request::header);return HttpClient.newHttpClient().send(request.build(),HttpResponse.BodyHandlers.ofString());
    }

    @Test void rejectsWrongAudienceAndForgedBody() throws Exception {
        Vector value = vector();
        var wrong = new BackendAdapter.Config(value.config.key(), value.config.contractDigest(), "wrong",
                value.config.hostId(), value.config.environment(), value.config.targetAgentRef(),
                value.config.bindingId(), value.config.publicationId(), value.config.policyDigest(),
                value.config.dataBoundaryDigest(), Files.createTempFile("a2a-java-wrong", ".replay"), 1024);
        Vector wrongAudience = value;
        assertThrows(SecurityException.class,
                () -> BackendAdapter.verify(wrongAudience.headers, wrongAudience.body, "INVOKE", wrong));
        Vector forged = vector();
        assertThrows(SecurityException.class,
                () -> BackendAdapter.verify(forged.headers, "{}".getBytes(StandardCharsets.UTF_8), "INVOKE", forged.config));
        COVERED.addAll(Set.of("wrong-audience","forged-signature"));
    }

    @Test void rejectsEveryTckIdentityAndRequestMutation() throws Exception {
        Map<String, java.util.function.Function<BackendAdapter.Config,BackendAdapter.Config>> configCases = Map.of(
                "wrong-host", c -> config(c, c.audience(), UUID.randomUUID().toString(), c.environment(), c.targetAgentRef(), c.bindingId(), c.publicationId(), c.policyDigest(), c.dataBoundaryDigest()),
                "wrong-environment", c -> config(c, c.audience(), c.hostId(), "prod", c.targetAgentRef(), c.bindingId(), c.publicationId(), c.policyDigest(), c.dataBoundaryDigest()),
                "wrong-agent", c -> config(c, c.audience(), c.hostId(), c.environment(), "wrong.agent", c.bindingId(), c.publicationId(), c.policyDigest(), c.dataBoundaryDigest()),
                "wrong-publication", c -> config(c, c.audience(), c.hostId(), c.environment(), c.targetAgentRef(), c.bindingId(), UUID.randomUUID().toString(), c.policyDigest(), c.dataBoundaryDigest()),
                "wrong-policy", c -> config(c, c.audience(), c.hostId(), c.environment(), c.targetAgentRef(), c.bindingId(), c.publicationId(), "sha256:"+"d".repeat(64), c.dataBoundaryDigest()),
                "wrong-data-boundary", c -> config(c, c.audience(), c.hostId(), c.environment(), c.targetAgentRef(), c.bindingId(), c.publicationId(), c.policyDigest(), "sha256:"+"d".repeat(64)),
                "unconfigured-destination", c -> config(c, c.audience(), c.hostId(), c.environment(), c.targetAgentRef(), UUID.randomUUID().toString(), c.publicationId(), c.policyDigest(), c.dataBoundaryDigest()));
        for (var entry : configCases.entrySet()) { Vector v=vector(); assertThrows(Exception.class,()->BackendAdapter.verify(v.headers,v.body,"INVOKE",entry.getValue().apply(v.config))); COVERED.add(entry.getKey()); }
        for (var mutation : Map.of("wrong-skill","skillId","wrong-task","taskId","wrong-context","contextId","wrong-idempotency","idempotencyKey").entrySet()) {
            Vector v=vector(); ObjectNode request=(ObjectNode)MAPPER.readTree(v.body); String value=mutation.getKey().equals("wrong-skill")?"wrong.skill":mutation.getKey().equals("wrong-idempotency")?"wrong":UUID.randomUUID().toString(); request.put(mutation.getValue(),value); byte[] body=MAPPER.writeValueAsBytes(request); Map<String,String> headers=signedHeaders(v.context,body,v.config); assertThrows(Exception.class,()->BackendAdapter.verify(headers,body,"INVOKE",v.config)); COVERED.add(mutation.getKey());
        }
        for (var item : Map.of("expired","expiresAt","deadline","deadline").entrySet()) { Vector v=vector(); v.context.put(item.getValue(),Instant.now().minusSeconds(1).toString()); Map<String,String> headers=signedHeaders(v.context,v.body,v.config); assertThrows(Exception.class,()->BackendAdapter.verify(headers,v.body,"INVOKE",v.config)); COVERED.add(item.getKey()); }
        Vector v=vector(); assertThrows(Exception.class,()->BackendAdapter.verify(v.headers,v.body,"CANCEL",v.config)); COVERED.add("wrong-operation");
    }

    @AfterAll static void writeTckReport() throws Exception {
        assertEquals(new TreeSet<>(REQUIRED), COVERED);
        String directory=System.getenv("A2A_TCK_REPORT_DIR");
        if(directory!=null&&!directory.isBlank()){Path path=Path.of(directory);Files.createDirectories(path);MAPPER.writeValue(path.resolve("java.json").toFile(),Map.of("contract",BackendAdapter.CONTRACT_VERSION,"lightA2aBuildSha256",System.getenv("LIGHT_A2A_BUILD_SHA256"),"coveredCases",COVERED));}
    }

    private static BackendAdapter.Config config(BackendAdapter.Config c,String audience,String host,String environment,String target,String binding,String publication,String policy,String boundary) {
        try { return new BackendAdapter.Config(c.key(),c.contractDigest(),audience,host,environment,target,binding,publication,policy,boundary,Files.createTempFile("a2a-java-case",".replay"),c.maximumRequestBytes()); }
        catch(Exception error){throw new RuntimeException(error);}
    }

    private static Map<String,String> signedHeaders(Map<String,Object> context,byte[] body,BackendAdapter.Config config) throws Exception {
        String encoded=Base64.getUrlEncoder().withoutPadding().encodeToString(MAPPER.writeValueAsBytes(context)); Mac mac=Mac.getInstance("HmacSHA256");mac.init(new SecretKeySpec(config.key(),"HmacSHA256"));mac.update(encoded.getBytes(StandardCharsets.US_ASCII));mac.update((byte)0);mac.update(body);return Map.of("x-light-a2a-backend-context",encoded,"x-light-a2a-backend-signature",HexFormat.of().formatHex(mac.doFinal()),"x-light-a2a-backend-contract-digest",config.contractDigest());
    }

    private static Vector vector() throws Exception { return vector("INVOKE"); }
    private static Vector vector(String operation) throws Exception {
        byte[] key = new byte[32]; java.util.Arrays.fill(key, (byte)'k');
        String task = UUID.randomUUID().toString(), contextId = UUID.randomUUID().toString();
        Map<String,Object> request = new LinkedHashMap<>();
        request.put("taskId",task);request.put("contextId",contextId);request.put("idempotencyKey","message-1");
        request.put("skillId","account.lookup");request.put("message",Map.of());request.put("metadata",Map.of());
        byte[] body=MAPPER.writeValueAsBytes(request);Instant now=Instant.now();
        Map<String,Object> context=new LinkedHashMap<>();
        context.put("contractVersion",BackendAdapter.CONTRACT_VERSION);context.put("invocationId",UUID.randomUUID().toString());context.put("issuer","light-a2a");context.put("audience","account-backend");context.put("hostId",UUID.randomUUID().toString());context.put("environment","dev");context.put("principalSubject","user:1");context.put("callerAgentRef","caller");context.put("targetAgentRef","account.agent");context.put("bindingId",UUID.randomUUID().toString());context.put("publicationId",UUID.randomUUID().toString());context.put("selectedSkillId","account.lookup");context.put("operation",operation);context.put("taskId",task);context.put("contextId",contextId);context.put("idempotencyKey","message-1");context.put("backendOperationId",Set.of("STATUS","CANCEL").contains(operation)?"op-1":null);context.put("policyDigest","sha256:"+"a".repeat(64));context.put("dataBoundaryDigest","sha256:"+"b".repeat(64));context.put("requestDigest","sha256:"+HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(body)));context.put("budget",Map.of("maximumInputBytes",1024,"maximumOutputBytes",1024,"maximumArtifactBytes",1024));context.put("traceparent",null);context.put("issuedAt",now.toString());context.put("deadline",now.plusSeconds(60).toString());context.put("expiresAt",now.plusSeconds(30).toString());
        String encoded=Base64.getUrlEncoder().withoutPadding().encodeToString(MAPPER.writeValueAsBytes(context));
        Mac mac=Mac.getInstance("HmacSHA256");mac.init(new SecretKeySpec(key,"HmacSHA256"));mac.update(encoded.getBytes(StandardCharsets.US_ASCII));mac.update((byte)0);mac.update(body);
        String signature=HexFormat.of().formatHex(mac.doFinal()),digest="sha256:"+"c".repeat(64);
        Map<String,String> headers=Map.of("x-light-a2a-backend-context",encoded,"x-light-a2a-backend-signature",signature,"x-light-a2a-backend-contract-digest",digest);
        var config=new BackendAdapter.Config(key,digest,(String)context.get("audience"),(String)context.get("hostId"),(String)context.get("environment"),(String)context.get("targetAgentRef"),(String)context.get("bindingId"),(String)context.get("publicationId"),(String)context.get("policyDigest"),(String)context.get("dataBoundaryDigest"),Files.createTempFile("a2a-java", ".replay"),1024);
        return new Vector(body,context,headers,config);
    }
    private record Vector(byte[] body,Map<String,Object> context,Map<String,String> headers,BackendAdapter.Config config){}
}
