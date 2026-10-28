"""Owned synthetic SDK wire endpoints. No AWS, credential, or business service."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json, threading
lock=threading.Lock();records=[];jwks=0
class Handler(BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def send(self,status,value,headers=None):
        data=json.dumps(value,separators=(',',':')).encode();self.send_response(status)
        for k,v in (headers or {}).items():self.send_header(k,v)
        self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
    def do_GET(self):
        global jwks
        if self.path=='/jwks':
            with lock:jwks+=1
            self.send(200,{'keys':[]})
        elif self.path=='/_state':
            with lock:self.send(200,{'invoke':list(records),'jwksFetches':jwks})
        else:self.send(200,{'ready':True})
    def do_POST(self):
        event=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
        with lock:records.append({'sdkPath':self.path,'eventPath':event.get('path'),'stage':event.get('requestContext',{}).get('stage'),'requestId':event.get('requestContext',{}).get('requestId'),'invocationType':self.headers.get('x-amz-invocation-type'),'logType':self.headers.get('x-amz-log-type'),'signed':bool(self.headers.get('authorization'))})
        if event.get('path')=='/backend-error':self.send(200,{'errorMessage':'synthetic business failure'},{'x-amz-function-error':'Unhandled'})
        else:self.send(200,{'statusCode':200,'body':'hello from business','headers':{'x-business':'yes'},'isBase64Encoded':False})
ThreadingHTTPServer(('0.0.0.0',19091),Handler).serve_forever()
