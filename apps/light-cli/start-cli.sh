#!/usr/bin/env bash
# Start the Light CLI against the local dev stack.
#
# The CLI is open source and downloadable anywhere, so nothing in it is a secret. You sign in as
# yourself, and every call to light-gateway and light-oauth is made with your own token.
#
# This file carries the long-lived DEV application token for com.networknt.light-cli-1.0.0 (env dev,
# scope portal.r portal.w), checked in on purpose so the CLI works out of the box against the local
# all-in-lt stack. It identifies the PROGRAM, not you, and is not a secret: it is what the platform
# services that ask "which application is this" (config-server today, controller-rs when the CLI
# registers with it) are given. It is never sent to light-gateway or light-oauth, and it authorises
# nothing on your behalf. Never put a staging or production token here.
#
# Usage:
#   ./start-cli.sh                 # open the Light CLI terminal; it stays open until /exit
#   ./start-cli.sh -c '/whoami'    # run one line and exit (repeat -c for several)
#   printf '/chat advisor\nhello\n' | ./start-cli.sh   # or pipe lines in
#   ./start-cli.sh --help
#
# Inside the terminal, everything is a slash command and anything else is said to the agent you
# are chatting with:
#   /login            sign in: shows a code to approve on the portal-view page
#   /whoami           who is signed in, and when the login ends
#   /agents           the agents you can chat with
#   /chat <agent>     chat with one (your login is refreshed as needed)
#   /tools            call light-gateway as you and list its tools
#   /logout           end the login on the server, delete the local tokens
#   /help, /exit
# LIGHT_USER_ACCESS_TOKEN, if set, is used instead of the signed-in session (development).
#
# Your login (access and refresh tokens) is kept in ~/.light/<env>/user-session.json, mode 0600
# (override the directory with LIGHT_HOME). Tokens are never taken from arguments or printed.
#
# Settings other than the environment and CA bundle live in config/cli.yml, and can be overridden
# by environment (CLI_GATEWAYURI, CLI_OAUTHURI, CLI_OAUTHPROVIDERID, CLI_OAUTHCLIENTID) or by the
# config server named in startup.yml. Signing in
# needs the device client set up in Portal (all-in-lt/device-authorization/README.md) and the
# Gateway routes for the OAuth paths.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"

export LIGHT_PORTAL_AUTHORIZATION="Bearer eyJhbGciOiJSUzI1NiIsImtpZCI6IkFacDAyTUN1Y3J1WmZGSmJ5eUZ3dWcifQ.eyJ0b2tlbl91c2UiOiJhcHAiLCJpc3MiOiJ1cm46Y29tOm5ldHdvcmtudDpvYXV0aDI6djEiLCJhdWQiOiJ1cm46Y29tLm5ldHdvcmtudCIsInN1YiI6IjAxYTBiZjgyLWU5MDAtNzczOS05M2IwLWYzM2MwNmRiNmVkYiIsImV4cCI6MjEwNTI3OTUzMiwianRpIjoiVGlkZTAweXhTVmlsWTlMWE5LZFJxZyIsImlhdCI6MTc4OTkxOTUzMiwibmJmIjoxNzg5OTE5NDEyLCJ2ZXIiOiIxLjAiLCJjaWQiOiIwMWEwYmY4Mi1lOTAwLTc3MzktOTNiMC1mMzNjMDZkYjZlZGIiLCJzY3AiOlsicG9ydGFsLnciLCJwb3J0YWwuciJdLCJjbGllbnRfaWQiOiIwMWEwYmY4Mi1lOTAwLTc3MzktOTNiMC1mMzNjMDZkYjZlZGIiLCJzY29wZSI6InBvcnRhbC53IHBvcnRhbC5yIiwiZW52IjoiZGV2IiwiaG9zdCI6IjAxOTY0YjA1LTU1MmEtN2M0Yi05MTg0LTY4NTdlN2YzZGM1ZiIsInNpZCI6ImNvbS5uZXR3b3JrbnQubGlnaHQtY2xpLTEuMC4wIn0.TijHBJNlRSaRSF6LN3caumS4ecyzFOesPvG8e2j9cj3IlqO5Bkt1gsMDBEy-ZZmlqHE5dSHLkidbWgtbxD2qLRfDG4i5WzlXHapni_CkO0lm3mv7N25qKVeqDafoKjs-MBMfuiGlR8QzF0gTXYLWpA4NxASTDIkzKQab4NXGPdpk511uuLHUef1bvpYho4l2eW6iEN0rYDUpl2hkdX5N-6icrKYW_EG4PNrrtzoyeOdmXLNCgA9ktEgZ23OWiKhtLW2jGqko-1LVO6oqJP35trVibZhZ5z3hhGL2z7OX7tBxB6z9n25qbWpQniV91DlxHTTT03CpKGNzoFGoRVyZ9A"

# The binary: LIGHT_CLI_BIN, else a release build, else a debug build.
BIN="${LIGHT_CLI_BIN:-}"
if [[ -z "$BIN" ]]; then
  for candidate in "$ROOT/target/release/light" "$ROOT/target/debug/light"; do
    if [[ -x "$candidate" ]]; then BIN="$candidate"; break; fi
  done
fi
if [[ -z "$BIN" ]]; then
  echo "light binary not found; building it (cargo build -p light-cli) ..." >&2
  (cd "$ROOT" && cargo build -p light-cli >&2)
  BIN="$ROOT/target/debug/light"
fi
if [[ ! -x "$BIN" ]]; then
  echo "light binary not found at $BIN" >&2
  exit 1
fi

exec "$BIN" --startup "${LIGHT_STARTUP_CONFIG:-$DIR/config/startup.yml}" "$@"
