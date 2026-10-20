These RSA private keys are public, test-only fixtures. Never use them outside tests.

The signing key is copied from the light-spa-4j PR #182 isolation fixture. The
other signing key is a separately generated 2048-bit RSA test key. Both JWKS
files deliberately use the same `kid` to exercise signing-trust isolation.
