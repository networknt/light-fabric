# Native admin binding inventory

The original native sample and the separate reconciled-full candidate retain
all six bindings. A missing alias is not a missing implementation. Configuration
repairs already add CORS/cache declarations; they do not admit deferred handlers.

| Native binding | Alias | Rust status |
|---|---|---|
| GET /health | health | Admitted Health factory; no admin chain |
| GET /adm/health | admin → health | Health factory admitted; full sample admin-chain prerequisites remain |
| GET /adm/server/info | admin → info | Admitted redacted module summary, verified JWT-only admin profile |
| GET /adm/logger | admin → getLogger | Admitted installed-filter summary, verified JWT-only admin profile |
| POST /adm/logger | admin → setLogger | Partial X-04 native POST/GET locally built-app-qualified; trusted gate + enabled logging + preceding verified JWT required; default selection rejects Init |
| GET /adm/cache/{name} | admin → cache | Explicitly deferred X-05; native alias declaration missing, Rust factory unavailable |

Native admin order is correlation → specification → security → validator. None
of these admin operations occurs in the original pet-store OpenAPI document.
Native Specification therefore also has an operation-selection prerequisite;
the full sample cannot be made compatible simply by implementing a terminal.
Validator remains enabled/unavailable in the full candidate. Supported-handler
isolation tests are not execution of this complete admin chain.

## LoggerSet comparison and deferral

Native `LoggerSetHandler` reads `LoggerConfig.enabled`. Disabled configuration
returns ERR10065; empty body returns ERR10059. For an enabled request it parses
a JSON array of name/level objects, looks up each Logback logger, changes its
explicit level sequentially and echoes the array in a JSON200 response. The
handler relies on its configured chain for authentication. Native LoggerGet
enumerates explicit logger name/level entries; Rust's approved GET instead
reports the actual installed tracing filter. Rust installs one process-owned,
currently immutable filter behind its privacy Subscriber. No setter provider or
reload controller exists.

Original deferral, before the approved partial exception below: the plan's
WP12 unsupported-configuration rule, Appendix A X-04 and Appendix B
LoggerSet row explicitly defer the setter. Lambda `local_activation` rejects
executable selection before plan compilation. A dormant setter declaration is
permitted; it does not activate a setter. Basic/API-key admin qualification is
also separately owner-deferred; current tests use the admitted JWT-only profile.

Smallest proposed scope: an explicit per-environment opt-in ROOT diagnostic
setter, INFO/DEBUG/TRACE only, existing verified-JWT admin contract, bounded
native-shaped command parser and real subscriber reload. This needs the one
decision in [LOGGERSET-SCOPE.md](/home/steve/workspace/artifacts/implementation/light-lambda/WP13-native-admin-scope-20261020T022204Z/LOGGERSET-SCOPE.md).
It would be partial X-04 support, not arbitrary Logback-registry equivalence.
That scope was subsequently approved and implemented for an isolated owned route.
The separate native reservation patch was then approved and qualified through
the built application; see [LOGGERSET.md](../../LOGGERSET.md). Default activation
remains closed. General LoggerSet behavior remains deferred.

## CacheExplorer comparison and deferral

Native `CacheExplorerHandler` is always enabled, reads the `name` path parameter
and obtains CacheManager from SingletonServiceFactory. It serializes the actual
named map; the special `jwk` case converts each value with `toString()`. Without
a manager it returns ERR11637. There is no handler-local redaction or JWT guard.

Rust has no equivalent generic cache-manager/inspection factory. Its token cache
bounds and private JWKS/principal caches are not permission to serialize their
contents. `cache.enabled=true` currently rejects earlier in the shared token/cache
configuration admission, before missing-factory admission. Named token-cache
support and the approved selected-Dynamo rejection do not lift X-05/K-01.
CacheExplorer remains deferred. A metadata-only inspector would be an intentional
new contract, not an implementation of native map serialization; it is not part
of the proposed LoggerSet scope.

## New authorized coverage

`tests/l1/native_admin.rs` uses real application factories, offline validation,
Init, built bootstrap Runtime API/service and owned dependencies:

- Selected LoggerSet nested through a chain rejects with GET enabled and disabled;
  zero Runtime API polls, Invokes and JWKS requests.
- Enabled CacheExplorer selection rejects at the real token/cache admission gate;
  zero polls/dependency calls. This is admission coverage, not cache execution.
- A separate admitted local-handler profile permits an unselected setter
  declaration. Nine cold/warm requests prove real Health, JWT-protected Info and
  GetLogger behavior,401 without a current principal,404 on deferred POST/cache
  attempts, unchanged actual filter after the mutation attempt, zero Invokes and
  one JWKS fetch. This does not alter or remove any sample binding.

Run from the worktree with fresh evidence:

```bash
rtk proxy env CARGO_INCREMENTAL=0 WP10_L1_EVIDENCE_DIR=/home/steve/workspace/artifacts/implementation/light-lambda/admin-rerun-$(date -u +%Y%m%dT%H%M%SZ) cargo test --locked --offline -p light-lambda --test l1 native_admin:: -- --test-threads=1
```

The new tests run the current built debug bootstrap, not a rebuilt delivery ZIP.
The delivered ZIP remains unchanged. Validator acceptance stays ready but
unexecuted until separate platform clearance is confirmed; paused experiments,
Router rewriting, AWS and full WP12/WP13 completion remain excluded.
