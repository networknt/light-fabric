# WP3 accepted fixture-contract coverage

123/130 rows have proposed Rust downstream reach assertions. The other seven are C-07/P-01a approved Java-only components, P-01/S-13d individually approved later qualification, and X-04/X-05/T-04 approved deferred-feature Init boundaries. The latter three have explicit Rust Init assertions; T-04 class-plugin rejection is not credited as an isolated missing-bounds cause. The owner-approved T-04 deferred boundary and D-08 bounded probes retain that distinction. No ingress rejection is credited as downstream policy coverage.

Rust execution evidence: zero. Structured clause assertions and Java evidence are linked in review/clause-level-acceptance-audit.json.

| Ledger | Java executed | Rust contract represented | Disposition |
| --- | --- | --- | --- |
| C-01 | True | True | required observable contract represented; Rust execution deferred |
| C-02 | True | True | required observable contract represented; Rust execution deferred |
| C-03 | True | True | required observable contract represented; Rust execution deferred |
| C-04 | True | True | required observable contract represented; Rust execution deferred |
| C-05 | True | True | required observable contract represented; Rust execution deferred |
| C-06 | True | True | required observable contract represented; Rust execution deferred |
| C-07 | True | False | approved Java-only representation |
| C-08 | True | True | required observable contract represented; Rust execution deferred |
| C-09 | True | True | required observable contract represented; Rust execution deferred |
| C-10 | True | True | required observable contract represented; Rust execution deferred |
| R-01 | True | True | required observable contract represented; Rust execution deferred |
| R-02 | True | True | required observable contract represented; Rust execution deferred |
| R-03 | True | True | required observable contract represented; Rust execution deferred |
| R-04 | True | True | required observable contract represented; Rust execution deferred |
| R-05 | True | True | required observable contract represented; Rust execution deferred |
| R-06 | True | True | required observable contract represented; Rust execution deferred |
| E-01 | True | True | required observable contract represented; Rust execution deferred |
| E-02 | True | True | required observable contract represented; Rust execution deferred |
| E-02a | True | True | required observable contract represented; Rust execution deferred |
| E-03 | True | True | required observable contract represented; Rust execution deferred |
| E-04 | True | True | required observable contract represented; Rust execution deferred |
| E-05 | True | True | required observable contract represented; Rust execution deferred |
| E-06 | True | True | required observable contract represented; Rust execution deferred |
| E-07 | True | True | required observable contract represented; Rust execution deferred |
| E-08 | True | True | required observable contract represented; Rust execution deferred |
| E-09 | True | True | required observable contract represented; Rust execution deferred |
| E-10 | True | True | required observable contract represented; Rust execution deferred |
| E-11 | True | True | required observable contract represented; Rust execution deferred |
| E-12 | True | True | required observable contract represented; Rust execution deferred |
| E-13 | True | True | required observable contract represented; Rust execution deferred |
| E-14 | True | True | required observable contract represented; Rust execution deferred |
| E-10a | True | True | required observable contract represented; Rust execution deferred |
| E-10b | True | True | required observable contract represented; Rust execution deferred |
| E-10c | True | True | required observable contract represented; Rust execution deferred |
| E-10d | True | True | required observable contract represented; Rust execution deferred |
| E-10e | True | True | required observable contract represented; Rust execution deferred |
| E-10f | True | True | required observable contract represented; Rust execution deferred |
| E-10g | True | True | required observable contract represented; Rust execution deferred |
| E-10h | True | True | required observable contract represented; Rust execution deferred |
| D-01 | True | True | required observable contract represented; Rust execution deferred |
| D-02 | True | True | required observable contract represented; Rust execution deferred |
| D-03 | True | True | required observable contract represented; Rust execution deferred |
| D-04 | True | True | required observable contract represented; Rust execution deferred |
| D-05 | True | True | required observable contract represented; Rust execution deferred |
| D-06 | True | True | required observable contract represented; Rust execution deferred |
| D-07 | True | True | required observable contract represented; Rust execution deferred |
| D-08 | True | True | required observable contract represented; Rust execution deferred |
| D-09 | True | True | required observable contract represented; Rust execution deferred |
| D-08a | True | True | required observable contract represented; Rust execution deferred |
| D-08b | True | True | required observable contract represented; Rust execution deferred |
| D-08c | True | True | required observable contract represented; Rust execution deferred |
| D-08d | True | True | required observable contract represented; Rust execution deferred |
| D-08e | True | True | required observable contract represented; Rust execution deferred |
| D-08f | True | True | required observable contract represented; Rust execution deferred |
| H-01 | True | True | required observable contract represented; Rust execution deferred |
| H-02 | True | True | required observable contract represented; Rust execution deferred |
| H-03 | True | True | required observable contract represented; Rust execution deferred |
| H-04 | True | True | required observable contract represented; Rust execution deferred |
| S-01 | True | True | required observable contract represented; Rust execution deferred |
| S-02 | True | True | required observable contract represented; Rust execution deferred |
| S-03 | True | True | required observable contract represented; Rust execution deferred |
| S-04 | True | True | required observable contract represented; Rust execution deferred |
| S-05 | True | True | required observable contract represented; Rust execution deferred |
| S-06 | True | True | required observable contract represented; Rust execution deferred |
| S-07 | True | True | required observable contract represented; Rust execution deferred |
| S-08 | True | True | required observable contract represented; Rust execution deferred |
| S-09 | True | True | required observable contract represented; Rust execution deferred |
| S-10 | True | True | required observable contract represented; Rust execution deferred |
| S-11 | True | True | required observable contract represented; Rust execution deferred |
| S-12 | True | True | required observable contract represented; Rust execution deferred |
| S-13 | True | True | required observable contract represented; Rust execution deferred |
| S-13a | True | True | required observable contract represented; Rust execution deferred |
| S-13b | True | True | required observable contract represented; Rust execution deferred |
| S-13c | True | True | required observable contract represented; Rust execution deferred |
| S-13d | False | False | individually approved later ownership |
| S-14 | True | True | required observable contract represented; Rust execution deferred |
| S-15 | True | True | deferred feature Init-rejection contract represented |
| S-16 | True | True | deferred feature Init-rejection contract represented |
| S-17 | True | True | required observable contract represented; Rust execution deferred |
| S-18 | True | True | required observable contract represented; Rust execution deferred |
| L-01 | True | True | required observable contract represented; Rust execution deferred |
| L-02 | True | True | required observable contract represented; Rust execution deferred |
| L-03 | True | True | required observable contract represented; Rust execution deferred |
| L-04 | True | True | required observable contract represented; Rust execution deferred |
| M-01 | True | True | required observable contract represented; Rust execution deferred |
| M-02 | True | True | deferred feature Init-rejection contract represented |
| M-03 | True | True | deferred feature Init-rejection contract represented |
| A-01 | True | True | required observable contract represented; Rust execution deferred |
| A-02 | True | True | required observable contract represented; Rust execution deferred |
| X-01 | True | True | required observable contract represented; Rust execution deferred |
| X-02 | True | True | required observable contract represented; Rust execution deferred |
| X-02a | True | True | required observable contract represented; Rust execution deferred |
| X-03 | True | True | required observable contract represented; Rust execution deferred |
| X-03a | True | True | required observable contract represented; Rust execution deferred |
| X-04 | True | True | deferred feature Init-rejection contract represented |
| X-05 | True | True | deferred feature Init-rejection contract represented |
| K-01 | True | True | deferred feature Init-rejection contract represented |
| CFG-01 | True | True | required observable contract represented; Rust execution deferred |
| CFG-02 | True | True | required observable contract represented; Rust execution deferred |
| CFG-03 | True | True | required observable contract represented; Rust execution deferred |
| CFG-04 | True | True | required observable contract represented; Rust execution deferred |
| CFG-05 | True | True | required observable contract represented; Rust execution deferred |
| P-01 | False | False | individually approved later ownership |
| T-01 | True | True | required observable contract represented; Rust execution deferred |
| T-02 | True | True | required observable contract represented; Rust execution deferred |
| T-03 | True | True | deferred feature Init-rejection contract represented |
| T-04 | True | True | approved deferred boundary represented; bounds-specific cause not isolated |
| V-01 | True | True | required observable contract represented; Rust execution deferred |
| V-02 | True | True | required observable contract represented; Rust execution deferred |
| V-03 | True | True | required observable contract represented; Rust execution deferred |
| Q-01 | True | True | required observable contract represented; Rust execution deferred |
| Q-02 | True | True | required observable contract represented; Rust execution deferred |
| B-01 | True | True | required observable contract represented; Rust execution deferred |
| B-02 | True | True | required observable contract represented; Rust execution deferred |
| B-03 | True | True | required observable contract represented; Rust execution deferred |
| B-04 | True | True | required observable contract represented; Rust execution deferred |
| B-05 | True | True | required observable contract represented; Rust execution deferred |
| B-02a | True | True | required observable contract represented; Rust execution deferred |
| B-03a | True | True | required observable contract represented; Rust execution deferred |
| P-01a | True | False | approved Java-only representation |
| S-01a | True | True | required observable contract represented; Rust execution deferred |
| E-06a | True | True | required observable contract represented; Rust execution deferred |
| S-07a | True | True | required observable contract represented; Rust execution deferred |
| E-10i | True | True | required observable contract represented; Rust execution deferred |
| M-01a | True | True | required observable contract represented; Rust execution deferred |
| V-03a | True | True | required observable contract represented; Rust execution deferred |
| Q-01a | True | True | required observable contract represented; Rust execution deferred |
| S-12a | True | True | required observable contract represented; Rust execution deferred |
| S-04a | True | True | required observable contract represented; Rust execution deferred |
| R-01a | True | True | required observable contract represented; Rust execution deferred |
