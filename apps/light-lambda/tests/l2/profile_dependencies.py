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


if __name__ == '__main__':
    ThreadingHTTPServer(('0.0.0.0', 19091), ProfileHandler).serve_forever()
