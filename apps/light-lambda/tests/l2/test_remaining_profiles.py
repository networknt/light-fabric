"""Regressions for new owned dependency observations and their privacy boundary."""
from http.server import ThreadingHTTPServer
import json
import threading
import unittest
import urllib.request
import remaining_dependencies as extra


class RemainingTransportTests(unittest.TestCase):
    def setUp(self):
        extra.base.state = {'invoke': [], 'http': [], 'jwksFetches': 0, 'oauth': [], 'extra': []}
        extra.base.settings = {'jwks': {'keys': []}}
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), extra.RemainingHandler)
        self.thread = threading.Thread(target=self.server.serve_forever)
        self.thread.start()
        self.url = 'http://127.0.0.1:' + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def test_oauth_records_grant_and_auth_presence_without_credentials(self):
        req = urllib.request.Request(self.url + '/token', b'grant_type=client_credentials',
                                     {'Authorization': 'Basic SECRET-DO-NOT-RECORD'})
        with urllib.request.urlopen(req, timeout=2) as response:
            self.assertEqual(json.load(response)['access_token'], extra.ACCESS)
        self.assertEqual(extra.base.state['oauth'], [{'path': '/token', 'method': 'POST',
                          'clientAuthPresent': True, 'clientCredentialsGrant': True}])
        self.assertNotIn('SECRET', json.dumps(extra.base.state))

    def test_outbound_token_roles_are_boolean_observations_only(self):
        value = {'headers': {'authorization': 'Bearer synthetic-subject', 'x-scope-token': 'Bearer ' + extra.ACCESS},
                 'multiValueHeaders': None, 'requestContext': {'requestId': 'synthetic-id'}, 'isBase64Encoded': True}
        req = urllib.request.Request(self.url + '/functions/test/invocations', json.dumps(value).encode())
        with urllib.request.urlopen(req, timeout=2) as response:
            self.assertEqual(json.load(response)['statusCode'], 200)
        observed = extra.base.state['extra'][0]
        self.assertTrue(observed['subjectPreserved'] and observed['secondaryAccessToken'] and observed['base64'])
        self.assertFalse(observed['outboundAccessToken'])
        self.assertNotIn(extra.ACCESS, json.dumps(extra.base.state))
        self.assertNotIn('synthetic-subject', json.dumps(extra.base.state))


if __name__ == '__main__':
    unittest.main()
