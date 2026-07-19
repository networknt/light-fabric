# Capacity, budget, and accounting incompleteness

1. Separate admission rejection, provider failure, and generator saturation.
   Check queue depth end-state before changing capacity.
2. Break usage down only by public alias and evidence quality. Missing provider
   usage must remain conservative and must not be relabelled as billing usage.
3. Confirm reserved cost reconciles or is retained under ambiguous acceptance.
   Pricing-unknown aliases remain ineligible.
4. For a capacity knee, reduce offered load or publish a reviewed concurrency
   change. Recovery requires stable admitted latency and zero residual backlog.
