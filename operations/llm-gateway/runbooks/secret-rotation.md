# Secret rotation and authentication storm

1. Stop rollout and inspect only credential generation/outcome metrics. Never
   place a credential reference or value in a label, query, or ticket.
2. Confirm the new reference is authorized for the host and materializes on all
   replicas. A failed candidate must leave the last valid root active.
3. Roll back to the prior credential reference when it remains valid; otherwise
   disable the affected deployment until a validated generation is published.
4. Confirm old client generations retire after in-flight requests drain and
   authentication failures return to baseline.
