"""Owned test-only transports. Persist only allowlisted synthetic observations."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading

lock = threading.Lock()
state = {'invoke': [], 'http': [], 'jwksFetches': 0}
settings = {'jwks': {'keys': []}}
SAFE = {'x-correlation-id', 'x-unrelated', 'x-remove', 'x-request', 'x-prefix', 'x-input', 'service_id'}


class ProfileHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, value, headers=None):
        data = json.dumps(value, separators=(',', ':')).encode()
        self.send_response(status)
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def route_http(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0'))).decode()
        with lock:
            state['http'].append({'path': self.path, 'method': self.command, 'body': body,
                                  'serviceIdPresent': self.headers.get('service_id') is not None})
        if '/disconnect' in self.path:
            self.close_connection = True
            return
        self.reply(302 if '/redirect' in self.path else 200, {'from': 'http'},
                   {'Location': 'http://127.0.0.1:1/do-not-follow'} if '/redirect' in self.path else None)

    def do_GET(self):
        if self.path.startswith('/base/'):
            self.route_http()
        elif self.path == '/jwks':
            with lock:
                state['jwksFetches'] += 1
                keys = settings['jwks']
            self.reply(200, keys)
        elif self.path == '/_state':
            with lock:
                self.reply(200, state)
        else:
            self.reply(200, {'ready': True})

    def do_POST(self):
        if self.path.startswith('/base/'):
            self.route_http()
            return
        event = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        if self.path == '/_configure':
            with lock:
                settings.update(event)
            self.reply(200, {'configured': True})
            return
        headers = {k.lower(): v for k, v in (event.get('headers') or {}).items() if k.lower() in SAFE}
        multi = {k.lower(): v for k, v in (event.get('multiValueHeaders') or {}).items() if k.lower() in SAFE}
        authorizer = event.get('requestContext', {}).get('authorizer') or {}
        with lock:
            state['invoke'].append({'path': event.get('path'), 'method': event.get('httpMethod'),
                                    'headers': headers, 'multiValueHeaders': multi, 'body': event.get('body'),
                                    'principalId': authorizer.get('principalId'),
                                    'subject': authorizer.get('claims', {}).get('sub'),
                                    'requestId': event.get('requestContext', {}).get('requestId'),
                                    'signed': bool(self.headers.get('authorization')),
                                    'logType': self.headers.get('x-amz-log-type')})
            reply = settings.get('backend')
        if event.get('path') == '/backend-error':
            self.reply(200, {'errorMessage': 'synthetic business failure'}, {'x-amz-function-error': 'Unhandled'})
        else:
            self.reply(200, reply or {'statusCode': 200, 'body': 'hello from business',
                                     'headers': {'x-business': 'yes'}, 'isBase64Encoded': False})



# Bounded test-only modes extend the existing owned profile endpoint. No AWS.
import base64,io,hashlib
MODES=__import__("json").loads('{\n  "success": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": "hello from business",\n      "headers": {\n        "x-business": "yes"\n      },\n      "isBase64Encoded": false\n    },\n    "functionError": null,\n    "expected": {\n      "status": 200,\n      "code": null,\n      "body": "hello from business",\n      "headers": {\n        "x-business": [\n          "yes"\n        ]\n      },\n      "isBase64Encoded": false\n    }\n  },\n  "backend-http500": {\n    "http": 200,\n    "payload": {\n      "statusCode": 500,\n      "body": "PUBLIC_BACKEND_500",\n      "headers": {\n        "x-business": "http500"\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 500,\n      "code": null,\n      "body": "PUBLIC_BACKEND_500",\n      "headers": {\n        "x-business": [\n          "http500"\n        ]\n      },\n      "isBase64Encoded": false\n    }\n  },\n  "backend-http403": {\n    "http": 200,\n    "payload": {\n      "statusCode": 403,\n      "body": "PUBLIC_BACKEND_DENIED"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 403,\n      "code": null,\n      "body": "PUBLIC_BACKEND_DENIED",\n      "headers": {},\n      "isBase64Encoded": false\n    }\n  },\n  "function-handled": {\n    "http": 200,\n    "payload": {\n      "errorType": "SyntheticFailure",\n      "errorMessage": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "stackTrace": [\n        "SYNTHETIC-PRIVATE-DETAIL-9a04"\n      ]\n    },\n    "functionError": "Handled",\n    "expected": {\n      "status": 502,\n      "code": "ERR14014"\n    }\n  },\n  "function-unhandled": {\n    "http": 200,\n    "payload": {\n      "errorType": "SyntheticFailure",\n      "errorMessage": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "stackTrace": [\n        "SYNTHETIC-PRIVATE-DETAIL-9a04"\n      ]\n    },\n    "functionError": "Unhandled",\n    "expected": {\n      "status": 502,\n      "code": "ERR14014"\n    }\n  },\n  "function-error-wins-valid-envelope": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": "Unhandled",\n    "expected": {\n      "status": 502,\n      "code": "ERR14014"\n    }\n  },\n  "function-error-wins-invalid-json": {\n    "http": 200,\n    "payload": null,\n    "functionError": "Unhandled",\n    "expected": {\n      "status": 502,\n      "code": "ERR14014"\n    },\n    "rawBase64": "U1lOVEhFVElDLVBSSVZBVEUtREVUQUlMLTlhMDQ="\n  },\n  "empty-payload": {\n    "http": 200,\n    "payload": null,\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    },\n    "rawBase64": ""\n  },\n  "malformed-json": {\n    "http": 200,\n    "payload": null,\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    },\n    "rawBase64": "e2JhZCBTWU5USEVUSUMtUFJJVkFURS1ERVRBSUwtOWEwNA=="\n  },\n  "invalid-utf8": {\n    "http": 200,\n    "payload": null,\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    },\n    "rawBase64": "/1NZTlRIRVRJQy1QUklWQVRFLURFVEFJTC05YTA0"\n  },\n  "null-payload": {\n    "http": 200,\n    "payload": null,\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "array-payload": {\n    "http": 200,\n    "payload": [],\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "string-payload": {\n    "http": 200,\n    "payload": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "missing-status": {\n    "http": 200,\n    "payload": {\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "null-status": {\n    "http": 200,\n    "payload": {\n      "statusCode": null,\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "string-status": {\n    "http": 200,\n    "payload": {\n      "statusCode": "200",\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "fractional-status": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200.5,\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "low-status": {\n    "http": 200,\n    "payload": {\n      "statusCode": 99,\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "high-status": {\n    "http": 200,\n    "payload": {\n      "statusCode": 600,\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "negative-status": {\n    "http": 200,\n    "payload": {\n      "statusCode": -1,\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "body-object": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": {\n        "nested": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "body-array": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": [\n        "SYNTHETIC-PRIVATE-DETAIL-9a04"\n      ]\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "invalid-base64": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": "%%%SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "isBase64Encoded": true\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "string-base64-flag": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "isBase64Encoded": "true"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "invalid-header-name": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "headers": {\n        "bad name": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "invalid-header-value": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "headers": {\n        "x-business": "bad\\nSYNTHETIC-PRIVATE-DETAIL-9a04"\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "invalid-multi-header-value": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "multiValueHeaders": {\n        "x-business": [\n          "bad\\nSYNTHETIC-PRIVATE-DETAIL-9a04"\n        ]\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "invalid-multi-header-type": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "multiValueHeaders": {\n        "x-business": "SYNTHETIC-PRIVATE-DETAIL-9a04"\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "invalid-discarded-scalar-header": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "headers": {\n        "X-Business": "bad\\nSYNTHETIC-PRIVATE-DETAIL-9a04"\n      },\n      "multiValueHeaders": {\n        "x-business": [\n          "valid"\n        ]\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14015"\n    }\n  },\n  "invoke-access-denied": {\n    "http": 403,\n    "payload": {\n      "Type": "User",\n      "message": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "__type": "AccessDeniedException"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14019"\n    }\n  },\n  "invoke-not-found": {\n    "http": 404,\n    "payload": {\n      "Type": "User",\n      "message": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "__type": "ResourceNotFoundException"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14019"\n    }\n  },\n  "invoke-service500": {\n    "http": 500,\n    "payload": {\n      "Type": "User",\n      "message": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "__type": "ServiceException"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14014"\n    }\n  },\n  "invoke-service503": {\n    "http": 503,\n    "payload": {\n      "Type": "User",\n      "message": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "__type": "ServiceException"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 502,\n      "code": "ERR14014"\n    }\n  },\n  "invoke-throttled": {\n    "http": 429,\n    "payload": {\n      "Type": "User",\n      "message": "SYNTHETIC-PRIVATE-DETAIL-9a04",\n      "__type": "TooManyRequestsException"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 429,\n      "code": "ERR14018"\n    }\n  },\n  "binary-response": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": "AAH/",\n      "isBase64Encoded": true,\n      "headers": {\n        "content-type": "application/octet-stream"\n      }\n    },\n    "functionError": null,\n    "expected": {\n      "status": 200,\n      "code": null,\n      "body": "AAH/",\n      "headers": {\n        "content-type": [\n          "application/octet-stream"\n        ]\n      },\n      "isBase64Encoded": true\n    }\n  },\n  "absent-body": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200\n    },\n    "functionError": null,\n    "expected": {\n      "status": 200,\n      "code": null,\n      "body": null,\n      "headers": {},\n      "isBase64Encoded": false\n    }\n  },\n  "null-body": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": null\n    },\n    "functionError": null,\n    "expected": {\n      "status": 200,\n      "code": null,\n      "body": null,\n      "headers": {},\n      "isBase64Encoded": false\n    }\n  },\n  "empty-body": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": ""\n    },\n    "functionError": null,\n    "expected": {\n      "status": 200,\n      "code": null,\n      "body": "",\n      "headers": {},\n      "isBase64Encoded": false\n    }\n  },\n  "null-base64-flag": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": "plain",\n      "isBase64Encoded": null\n    },\n    "functionError": null,\n    "expected": {\n      "status": 200,\n      "code": null,\n      "body": "plain",\n      "headers": {},\n      "isBase64Encoded": false\n    }\n  },\n  "nested-status-text": {\n    "http": 200,\n    "payload": {\n      "statusCode": 200,\n      "body": "{\\"nested\\":{\\"statusCode\\":\\"not-a-status\\"}}"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 200,\n      "code": null,\n      "body": "{\\"nested\\":{\\"statusCode\\":\\"not-a-status\\"}}",\n      "headers": {},\n      "isBase64Encoded": false\n    }\n  },\n  "headers-multimap-overlap": {\n    "http": 200,\n    "payload": {\n      "statusCode": 201,\n      "headers": {\n        "Content-Type": "ignored",\n        "X-Scalar": "yes",\n        "SET-COOKIE": "ignored"\n      },\n      "multiValueHeaders": {\n        "content-type": [\n          "text/plain"\n        ],\n        "Set-Cookie": [\n          "a=1",\n          "b=2"\n        ]\n      },\n      "body": "created"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 201,\n      "code": null,\n      "body": "created",\n      "headers": {\n        "content-type": [\n          "text/plain"\n        ],\n        "x-scalar": [\n          "yes"\n        ],\n        "set-cookie": [\n          "a=1",\n          "b=2"\n        ]\n      },\n      "isBase64Encoded": false\n    }\n  },\n  "status599": {\n    "http": 200,\n    "payload": {\n      "statusCode": 599,\n      "body": "PUBLIC_STATUS599"\n    },\n    "functionError": null,\n    "expected": {\n      "status": 599,\n      "code": null,\n      "body": "PUBLIC_STATUS599",\n      "headers": {},\n      "isBase64Encoded": false\n    }\n  }\n}\n')
state.update({'backendModes': [], 'jwksEvents': []})
settings.update({'mode':'success','jwksMode':'valid'})
class ExtendedHandler(ProfileHandler):
    def do_GET(self):
        if self.path != '/jwks':return super().do_GET()
        with lock:
            state['jwksFetches']+=1
            mode=settings.get('jwksMode','valid');keys=settings['jwks']
            state['jwksEvents'].append({'ordinal':state['jwksFetches'],'mode':mode,'completed':False})
        if mode=='503':status,raw=503,b'SYNTHETIC-PRIVATE-JWKS-DETAIL-9a04'
        elif mode=='malformed':status,raw=200,b'{SYNTHETIC-PRIVATE-JWKS-DETAIL-9a04'
        else:status,raw=200,json.dumps({'keys':[]} if mode=='empty' else keys,separators=(',',':')).encode()
        self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw);self.wfile.flush()
        with lock:state['jwksEvents'][-1]['completed']=True
    def do_POST(self):
        if not self.path.endswith('/invocations'):return super().do_POST()
        raw=self.rfile.read(int(self.headers.get('Content-Length','0')))
        event=json.loads(raw)
        with lock:
            mode=settings.get('mode','success')
            state['backendModes'].append({'mode':mode,'sdkPath':self.path,'requestId':event.get('requestContext',{}).get('requestId'),'base64':event.get('isBase64Encoded'),'completed':False})
        self.rfile=io.BytesIO(raw)
        return super().do_POST()
    def reply(self,status,value,headers=None):
        if not self.path.endswith('/invocations'):return super().reply(status,value,headers)
        with lock:mode=settings.get('mode','success')
        choice=MODES[mode]
        raw=base64.b64decode(choice['rawBase64']) if 'rawBase64' in choice else json.dumps(choice['payload'],separators=(',',':')).encode()
        self.send_response(choice['http']);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)))
        if choice['functionError']:self.send_header('x-amz-function-error',choice['functionError'])
        self.send_header('x-amz-executed-version','1');self.end_headers();self.wfile.write(raw);self.wfile.flush()
        with lock:
            record=state['backendModes'][-1];record.update(completed=True,responseStatus=choice['http'],functionError=choice['functionError'],responseBytes=len(raw),responseSha256=hashlib.sha256(raw).hexdigest())
if __name__=='__main__':ThreadingHTTPServer(('0.0.0.0',19091),ExtendedHandler).serve_forever()
