#!/usr/bin/env python3
"""Check both synchronous Lambda Invoke metadata and the gateway application response."""
import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('metadata', type=Path)
    parser.add_argument('response', type=Path)
    parser.add_argument('--status', type=int, required=True)
    args = parser.parse_args()
    metadata = json.loads(args.metadata.read_text())
    response = json.loads(args.response.read_text())
    if metadata.get('StatusCode') != 200 or metadata.get('FunctionError'):
        raise SystemExit('Lambda Invoke failed: inspect the saved metadata/response privately')
    if response.get('statusCode') != args.status:
        raise SystemExit('Unexpected application status: ' + str(response.get('statusCode')))
    if not isinstance(response.get('body'), str):
        raise SystemExit('Missing gateway response body')
    print(json.dumps({'invokeStatus': metadata['StatusCode'], 'functionError': None,
                      'applicationStatus': response['statusCode']}))


if __name__ == '__main__':
    main()
