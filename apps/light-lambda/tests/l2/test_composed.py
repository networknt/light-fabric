"""Focused regressions for composed-profile owned dependency observations."""
from http.server import ThreadingHTTPServer
import json
import threading
import unittest
import urllib.request
import composed_dependencies as fake


class ComposedDependencyTests(unittest.TestCase):
    def setUp(self):
        fake.base.state = {'invoke': [], 'http': [], 'jwksFetches': 0, 'oauth': [], 'extra': [],
                           'composedInvoke': [], 'composedHttp': []}
        fake.base.settings = {'jwks': {'keys': []}}
        self.before_access = fake.remaining.ACCESS
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), fake.ComposedDependencies)
        self.thread = threading.Thread(target=self.server.serve_forever)
        self.thread.start()
        self.url = 'http://127.0.0.1:' + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        fake.remaining.ACCESS = self.before_access

    def post(self, path, value):
        req = urllib.request.Request(self.url + path, json.dumps(value).encode())
        with urllib.request.urlopen(req, timeout=2) as response:
            return json.load(response)

    def test_owned_oauth_can_issue_signed_fixture_without_persisting_value(self):
        self.post('/_configure', {'oauthAccessToken': 'SECRET-SYNTHETIC-SIGNED-WITNESS'})
        req = urllib.request.Request(self.url + '/token', b'grant_type=client_credentials')
        with urllib.request.urlopen(req, timeout=2) as response:
            self.assertEqual(json.load(response)['access_token'], 'SECRET-SYNTHETIC-SIGNED-WITNESS')
        self.assertNotIn('SECRET-SYNTHETIC-SIGNED-WITNESS', json.dumps(fake.base.state))

    def test_sdk_observation_only_adds_allowlisted_query_and_header_fields(self):
        self.post('/functions/test/invocations', {'path': '/changed',
            'queryStringParameters': {'added': '7', 'secret': 'DO-NOT-RECORD'},
            'headers': {'x-added': 'updated', 'authorization': 'Bearer DO-NOT-RECORD'}})
        self.assertEqual(fake.base.state['composedInvoke'], [{'path': '/changed', 'addedQuery': '7'}])
        self.assertEqual(fake.base.state['invoke'][0]['headers'], {'x-added': 'updated'})
        self.assertNotIn('DO-NOT-RECORD', json.dumps(fake.base.state))


if __name__ == '__main__':
    unittest.main()
