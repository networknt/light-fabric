# Slow consumers

1. Check active stream permits, downstream write progress, minimum-drain and
   idle terminations, TTFT, and upstream cancellation outcomes.
2. Confirm bounded channels are not growing and disconnected clients release
   global, principal, alias, stream, and deployment permits.
3. Do not increase channel capacity or deadlines during the incident. Identify
   the client/network cohort outside metric labels, then shed or rate-limit it.
4. Verify healthy traffic recovers without residual permits or backlog.
