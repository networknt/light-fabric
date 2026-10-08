# Portal runtime configuration fixtures

Copied byte-for-byte from portal-view/contracts/portal-config/fixtures at
commit 0c9d7e80cd178ab97e91605a68b1223405f0f7f4 (WP7).
All 33 JSON paths and SHA-256 hashes match the source: 6 valid, 27 invalid.
Directory names define expectations; browser fixtures were not changed.
The test-support schema literal in spa.rs is copied from public/portal-config.schema.json
at the same commit. It is signed into temporary test releases, using throwaway
ring Ed25519 keys created in test temporary directories. No production trust.
