"""Owned test-only composed-chain transport observations, no credential values."""
import io
import json
from http.server import ThreadingHTTPServer
import profile_dependencies as base
import remaining_dependencies as remaining

base.SAFE.add('x-added')
base.state.update({'composedInvoke': [], 'composedHttp': []})


class ComposedDependencies(remaining.RemainingHandler):
    def route_http(self):
        with base.lock:
            base.state['composedHttp'].append({'path': self.path,
                'added': self.headers.get('x-added'), 'request': self.headers.get('x-request'),
                'secondaryAccessToken': self.headers.get('x-scope-token') == 'Bearer ' + remaining.ACCESS,
                'outboundAccessToken': self.headers.get('authorization') == 'Bearer ' + remaining.ACCESS,
                'correlation': self.headers.get('x-correlation-id')})
        super().route_http()

    def do_POST(self):
        if self.path == '/_configure':
            raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            value = json.loads(raw)
            if 'oauthAccessToken' in value:
                # Only the owned fake reply changes; never persist token values.
                remaining.ACCESS = value['oauthAccessToken']
            self.rfile = io.BytesIO(raw)
        if self.path.endswith('/invocations'):
            raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            value = json.loads(raw)
            with base.lock:
                base.state['composedInvoke'].append({'path': value.get('path'),
                    'addedQuery': (value.get('queryStringParameters') or {}).get('added')})
            self.rfile = io.BytesIO(raw)
        super().do_POST()


if __name__ == '__main__':
    ThreadingHTTPServer(('0.0.0.0', 19091), ComposedDependencies).serve_forever()
