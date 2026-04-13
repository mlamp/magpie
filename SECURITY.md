# Security Policy

## Scope

magpie parses bytes from untrusted sources (`.torrent` files, tracker responses, peer wire messages, DHT traffic). Any parser that crashes, panics on valid-looking input, reads out of bounds, or allocates unbounded memory is in scope. So is any code path that can be coerced into writing outside the intended storage region, leaking memory, or consuming CPU without bound.

Out of scope: issues in consumers (e.g. lightorrent), issues in the reference implementations we study under `_tmp/`, and attacks that require local filesystem/root access.

## Reporting

Until a dedicated channel is set up, please report vulnerabilities via GitHub's private security advisory feature on the `mlamp/magpie` repository (`Security` → `Report a vulnerability`). Do **not** open a public issue for a suspected vulnerability.

Expected acknowledgement: within 5 working days. We will coordinate a disclosure timeline once the issue is triaged.

## Supported versions

Pre-1.0: only the latest released minor line receives fixes. Post-1.0: the current and previous minor lines.
