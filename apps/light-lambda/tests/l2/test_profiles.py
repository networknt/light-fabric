"""Focused regressions for the owned test transport, not handler substitutes."""
from http.server import ThreadingHTTPServer
import json
import threading
import unittest
import urllib.request

import profile_dependencies as fake


class ProfileTransportTests(unittest.TestCase):
    def setUp(self):
        fake.state = {'invoke': [], 'http': [], 'jwksFetches': 0}
        fake.settings = {'jwks': {'keys': []}}
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), fake.ProfileHandler)
        self.thread = threading.Thread(target=self.server.serve_forever)
        self.thread.start()
        self.url = 'http://127.0.0.1:' + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def post(self, value):
        req = urllib.request.Request(self.url + '/functions/test/invocations',
                                     json.dumps(value).encode(), {'Content-Type': 'application/json'})
        with urllib.request.urlopen(req, timeout=2) as response:
            return json.load(response)

    def test_nullable_headers_and_authorizer_are_observed_without_transport_failure(self):
        response = self.post({'headers': None, 'multiValueHeaders': None,
                              'requestContext': {'authorizer': None}, 'path': '/fallback'})
        self.assertEqual(response['statusCode'], 200)
        self.assertEqual(fake.state['invoke'][0]['headers'], {})
        self.assertIsNone(fake.state['invoke'][0]['principalId'])

    def test_observations_exclude_credentials_and_unlisted_headers(self):
        self.post({'headers': {'authorization': 'DO-NOT-RECORD', 'x-request': 'synthetic',
                               'x-api-key': 'DO-NOT-RECORD'},
                   'multiValueHeaders': {'authorization': ['DO-NOT-RECORD']},
                   'requestContext': {'authorizer': {'principalId': 'synthetic-A',
                                                     'claims': {'sub': 'synthetic-A', 'private': 'DO-NOT-RECORD'}}}})
        rendered = json.dumps(fake.state)
        self.assertNotIn('DO-NOT-RECORD', rendered)
        self.assertEqual(fake.state['invoke'][0]['headers'], {'x-request': 'synthetic'})


if __name__ == '__main__':
    unittest.main()
