"""Additional owned dependency observations; no credential values are persisted."""
import io
import json
from http.server import ThreadingHTTPServer
import profile_dependencies as base

base.state.update({'oauth': [], 'extra': []})
base.SAFE.add('x-traceability-id')
ACCESS = 'SYNTHETIC-L2-ACCESS-NOT-A-REAL-TOKEN'


class RemainingHandler(base.ProfileHandler):
    def do_POST(self):
        if self.path == '/token':
            raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            with base.lock:
                base.state['oauth'].append({'path': self.path, 'method': self.command,
                                            'clientAuthPresent': self.headers.get('authorization', '').startswith('Basic '),
                                            'clientCredentialsGrant': raw == b'grant_type=client_credentials'})
                mode = base.settings.get('tokenMode', 'valid')
            if mode == 'denied':
                self.reply(401, {'statusCode': 401, 'code': 'ERR10000', 'message': 'INVALID_AUTH_TOKEN',
                                 'severity': 'ERROR', 'description': 'synthetic denial'})
            elif mode == 'malformed':
                self.reply(401, {'error': 'invalid_client'})
            else:
                self.reply(200, {'access_token': ACCESS, 'expires_in': 3600})
            return
        if self.path.endswith('/invocations'):
            raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            event = json.loads(raw)
            headers = {k.lower(): v for k, v in (event.get('headers') or {}).items()}
            auth = headers.get('authorization')
            with base.lock:
                base.state['extra'].append({'requestId': event.get('requestContext', {}).get('requestId'),
                                            'outboundAccessToken': auth == 'Bearer ' + ACCESS,
                                            'secondaryAccessToken': headers.get('x-scope-token') == 'Bearer ' + ACCESS,
                                            'subjectPreserved': auth == 'Bearer synthetic-subject',
                                            'authorizationPresent': auth is not None,
                                            'base64': event.get('isBase64Encoded')})
            self.rfile = io.BytesIO(raw)
        super().do_POST()


if __name__ == '__main__':
    ThreadingHTTPServer(('0.0.0.0', 19091), RemainingHandler).serve_forever()
