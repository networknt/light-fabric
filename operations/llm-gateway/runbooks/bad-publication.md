# Bad publication or version mismatch

1. Stop expansion; retain the last valid snapshot and compare replica digest,
   sequence, schema version, and minimum gateway version.
2. Check delta-gap/resync and acknowledgement failures. Do not edit cache files.
3. Republish the prior signed manifest. Confirm every replica converges and the
   retained-generation count returns to its normal bound.
4. If compiler/schema rejection persists, disable `llm-router.enabled` and
   preserve the rejected manifest plus sanitized acknowledgement as evidence.
