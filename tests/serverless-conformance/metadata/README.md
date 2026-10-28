# Reviewed declaration metadata

`wp13-declarations-v1.json` reviews exactly 76 conflicts recorded by the preceding WP13 run. Each entry binds the original fixture digest and declarations, unchanged per-runner expectation equality for each removed field, original Java/Rust result identities, and CP1 ledger references. It changes no expectation or fixture bytes.

The runner rechecks the fixture digest, original declaration set and equality of every removed field against both original expectations. It then uses the same strict comparison engine. A new observed difference still fails; failed expectations never become passes. Newly exposed conflicts are not corrected automatically.

`compare` uses reviewed metadata by default. `--declarations original` reproduces the historical declarations. Reports expose the mode, applied review, declaration conflicts, differential mismatches and expectation failures independently. The result-v1 schema and skipped contract are unchanged.

Individual review evidence: `/home/steve/workspace/artifacts/implementation/light-lambda/WP13-failures-declarations-20261018T132733Z/declaration-review.md` and `.json`.

## Closure overlay v2

`wp13-declarations-v2.json` adds15 individually reviewed closure conflicts and1 additional H-04 conflict exposed by the real Init-assertion fix. Fresh native Java evidence passes every affected case. An Init observation has no ALB response header namespace; this does not relax header assertions on response observations. Both overlays are digest-bound and retain original expectation equality checks.


## Approved Rust expectations v1

The owner approved all eleven exact replacements in the preserved
`wp13-rust-expectations-proposal-v1.json`. The active copy is
`wp13-rust-expectations-v1.json`; the runner verifies its source-package SHA256,
original fixture digest, original expectation array and exact approved replacement
before applying it in memory. Original fixtures, Java expectations, both DIGESTs
and earlier declaration reviews remain unchanged.

`--expectations approved-v1` is the default; `--expectations original` reproduces
original Rust expectations. Results retain original fixture digests and result-v1
semantics; the coverage sidecar records the amendment and mode. The original
Java fallback is captured before replacing any Rust expectation.

The eleven `approvedExpectationDifferenceFields` sets are checked against the
preserved Java and approved Rust expectation pair on every review. They cannot
be inferred from observed output. Original source declarations remain in coverage
alongside `sourceDeclarationConflictsSupersededByApprovedExpectation`;
`--declarations original` can still expose them. Strict comparison asserts both
runner expectations and rejects any additional observed difference.

Init and ingress rejections are `admission-only`, separately counted from
`service-execution`. Neither category alone proves a particular declared handler
executed; valid-input built-app tests and dispatch assertions establish limiter,
Transformer and Router transport behavior. Incomplete Router rewrite profiles
remain incomplete even when their Init rejection matches an admission contract.
The preserved Java L-02 timing failure remains a failed comparison.


## Registered family adapters v1 and declaration overlay v3

`wp13-family-adapters-v1.json` selects65 additional fixtures in six families:
JWT/unified security/scope54, Token/OAuth3, Transformer3, Limiter3,
Correlation/headers1 and Proxy metadata1. `apps/light-lambda/scripts/compare-family.py`
runs these selections using the existing real Init/service/SDK runner and strict
comparator. It requires fresh output directories and keeps nonzero status for
failed expectations or comparisons. Family selection is not full-package coverage.

`wp13-declarations-v3.json` contains38 digest-bound metadata reviews. Every removed
field has equal unchanged original Java/Rust expectations and matching successful
native Java and Rust execution evidence. Init expectations are checked too when
both sides declare them; bootstrap differences cannot be removed merely because
per-request fetch counts are equal. The source declarations, all fixtures,
previous overlays and approved Rust expectations remain unchanged. Three S-14
bootstrap-count differences remain undeclared and are not waived by v3.

JWT recipes generate ephemeral RSA keys and compact signatures in memory. Owned
JWKS endpoints record every request, including undeclared routes; warm key changes
and signAtStep follow native fixture recipes. Production Init performs bootstrap,
and the real guarded service performs verification, scopes and cache handling.
Generated tokens are replaced by recipe references before result/error persistence.
Bootstrap and per-request fetch/call observations are independently asserted.

The closed OAuth prerequisite prepares only the named token cache (15min/max100)
with the unrelated cache-explorer handler disabled. It never preloads a token.
Production acquisition and warm caching execute over the owned HTTP dependency.
The wrapper scopes `LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS=1` to its child
runner process; the provider still enforces loopback destinations. OAuth header
names alone use native case-insensitive semantics; values and multiplicity stay
exact, and raw recorded headers are unchanged.

Clock and generated-correlation adapters assert only the existing closed recipes:
request epoch inside the actual invocation window, coupled exact UTC text, and
base64url128 identity coupled across the explicitly declared scalar/multi-value
maps. They introduce no arbitrary masks. The first invocation uses the native
synthetic request ID; later warm invocations retain distinct synthetic identities.
Init/ingress rejection remains admission-only, and a dispatched business response
cannot earn admission coverage merely by containing an ingress error code.
