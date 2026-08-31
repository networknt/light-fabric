from light_a2a_backend import AdapterConfig, ReplayStore, serve
from pathlib import Path

class AccountAgent:
    def capabilities(self): return {"contractVersion":"light-a2a-backend/v1","streaming":False,"cancellation":True,"statusReconciliation":True,"acceptedContentModes":["text/plain"],"maximumArtifactBytes":1048576}
    def invoke(self, _context, request): return {"state":"COMPLETED","backendOperationId":"account:"+request["taskId"],"result":{"answer":"business result"},"error":None,"artifacts":[]}
    def invoke_stream(self, _context, _request): return iter(())
    def status(self, _context, request): return self.invoke(_context,request)
    def cancel(self, _context, request): return {"state":"CANCELED","backendOperationId":"account:"+request["taskId"],"result":None,"error":None,"artifacts":[]}

# Deployment supplies AdapterConfig from its generated business-agent config;
# business code above has no HTTP, signature, replay, token, or Portal logic.
