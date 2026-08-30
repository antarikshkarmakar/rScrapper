# Security policy

## Supported versions

| Version | Security fixes |
| --- | --- |
| 0.2.x | Supported |
| 0.1.x and earlier | Unsupported |

Report a suspected vulnerability privately to
[antariksh.karmakar@gmail.com](mailto:antariksh.karmakar@gmail.com). Include the
affected component/version, prerequisites, a minimal reproduction, and impact.
Do not include real credentials, cookies, private page bodies, or another
person's data. Please allow time for triage before public disclosure.

## Trust model

Remote pages, DNS, redirects, search results, social-provider responses, model
responses, MCP tool output, and generated reports are untrusted. rScrapper
reduces several transport and resource risks, but callers remain responsible
for authorization, provider terms, output review, deployment egress, and any
action taken from scraped or model-generated text.

## Destination and SSRF policy

The default `NetworkPolicy::PublicInternet` accepts credential-free `http` and
`https` URLs only. It rejects local names and non-public destinations before a
request and again during DNS resolution. If any address in a DNS answer is
forbidden, the whole answer is rejected.

Forbidden IPv4 ranges are `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`,
`127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`,
`192.0.2.0/24`, `192.88.99.0/24`, `192.168.0.0/16`, `198.18.0.0/15`,
`198.51.100.0/24`, `203.0.113.0/24`, `224.0.0.0/4`, and
`240.0.0.0/4`.

IPv6 must be globally scoped `2000::/3`. IPv4-compatible and mapped addresses
are classified using their IPv4 value. Translation, discard, special-purpose,
documentation, ORCHID, 6to4, deprecated, unique-local, link-local, site-local,
and multicast ranges are rejected, including `64:ff9b::/96`,
`64:ff9b:1::/48`, `100::/64`, `2001::/23`, `2001:2::/48`,
`2001:db8::/32`, `2001:10::/28`, `2001:20::/28`, `2002::/16`,
`3ffe::/16`, `3fff::/20`, `fc00::/7`, `fe80::/10`, `fec0::/10`, and
`ff00::/8`.

Every redirect target is revalidated; the core follows at most 10 redirects.
URL credentials and non-HTTP(S) schemes are rejected. The explicit
`AllowPrivate` policy exists for trusted library/local diagnostic fixtures;
the public API never accepts a request parameter that enables it.

## TLS and proxies

HTTP clients use rustls with certificate and hostname verification enabled.
There is no configuration switch to accept an unverified peer. Proxy URLs are
validated and redacted from diagnostics. Tor-required paths accept `socks5h`
so proxy-side name resolution is preserved; they do not fall back to a direct
connection.

## HTTP API boundary

The API defaults to `127.0.0.1:8787`. A non-loopback `RSCRAPER_BIND` is
rejected before listen unless `RSCRAPER_API_TOKEN` is present and consists only
of visible non-whitespace ASCII. Operation routes compare a single
`Authorization: Bearer <token>` value in constant time. `/health` remains
unauthenticated.

Hard boundaries are:

- JSON request body: 64 KiB;
- serialized response: 10 MiB;
- scrape/search deadline: 30 seconds;
- crawl deadline: 120 seconds;
- concurrent operation permits: default 8, configurable 1–32;
- search results: 1–20, default 5;
- crawl pages: 1–100, default 20;
- crawl concurrency: 1–16, default 4.

Unknown JSON fields, malformed inputs, over-limit work, and exhausted operation
permits produce bounded errors. Graceful shutdown handles `Ctrl-C` and Unix
`SIGTERM`.

## Core resource bounds

Default fetch limits are a 10-second connect timeout, a 30-second complete
request deadline, 5 MiB of decoded response body, 1,000,000 Unicode scalar
values of rendered output, and 10 redirects. Bodies are stopped while streaming.
Crawler and parser work has separate hard maxima documented in public types and
validated before scheduling.

## Cookie and local state handling

`RSCRAPER_HOME` defaults to `$HOME/.rscraper`. On Unix the state directory is
created with owner-only mode `0700`. Cookie inputs must be regular files, must
not be symlinks, and must have mode `0600`; group/other permissions are
rejected. `PlatformCookieJar` redacts values from `Debug`, and errors/logs do
not include raw cookie or authorization contents.

Cookies are loaded by a named platform adapter with an origin restriction.
Operators should still treat cookie files as credentials, supply the minimum
scope needed, review adapter destinations, rotate expired material, and keep
state directories out of backups and artifacts unless those systems are
equally protected.

## Browser isolation and cleanup

Each render owns a fresh temporary profile, a policy-enforcing loopback proxy,
controller tasks, and a Chromium child. The renderer:

- keeps Chromium's OS sandbox enabled;
- disables downloads, extensions, background networking, and unnecessary media;
- applies the configured direct-policy or SOCKS egress;
- validates intercepted destinations and redirect hops;
- limits navigation and captured DOM size;
- terminates and reaps the child on success, error, deadline, or cancellation;
- closes the owned proxy listener, drains owned connections/controller tasks,
  marks terminal lifecycle state, and removes the temporary profile.

Executable discovery does not turn the browser into a general automation API.
Real-browser tests are ignored by default because a compatible local
Chromium/Chrome installation is not part of deterministic CI.

## Tor and Robin

Robin validates the `socks5h` endpoint and establishes its Tor transport before
the first model call. Search, source retrieval, and optional browser rendering
use that transport. Browser redirects are restricted to the original host;
failure is terminal rather than a direct retry.

Robin makes at most three provider calls, retrieves at most five source pages,
and enforces prompt, response, URL, and report bounds. Provider keys are supplied
through `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY`;
`OLLAMA_HOST` selects an Ollama endpoint.

## Prompt injection and untrusted output

MCP output begins with a visible untrusted-data warning, delimits remote
content, and prefixes every remote line. Robin delimits and escapes search/source
blocks and labels model summaries as untrusted. These controls reduce accidental
instruction mixing; they do not prove that a model will ignore adversarial text.
Keep tool permissions narrow, require human review for consequential actions,
and verify report claims independently.

## Unsupported protection claims

rScrapper is not an anti-bot evasion product. It does not solve CAPTCHA,
challenge, paywall, consent, checkpoint, or provider-authentication pages.
Browser rendering is a bounded rendering fallback, not proof that a target is
accessible or that its use is authorized.

## Logging and errors

Logs go to stderr; MCP reserves stdout for JSON-RPC. Structured diagnostics omit
URLs or redact sensitive URL values where the contract requires it, and do not
emit cookies, authorization headers, provider keys, proxy credentials, request
bodies, private response bodies, or model prompt content. Error values expose
stable categories and bounded metadata rather than raw upstream secrets.

## Dependency assurance

The release gate uses the locked dependency graph, Rust 1.88.0, Clippy with
warnings denied, deterministic tests/docs, and `cargo-audit 0.22.2` with audit
warnings denied. Release preparation must stop on a vulnerability, an
unresolved advisory/yanked warning, MSRV drift, or a dependency change that
weakens rustls-only transport.

## Operator rollback triggers

Stop traffic and roll back to the last reviewed build if any of these is
observed:

- a non-loopback API listener starts without required authentication, or bearer
  checks can be bypassed;
- a cookie, token, provider key, prompt, private body, or proxy credential appears
  in logs, errors, reports, caches, build artifacts, or protocol diagnostics;
- destination policy, DNS-answer rejection, redirect validation, TLS
  verification, Tor enforcement, or same-host rendering is bypassed;
- a cancelled/timed-out browser leaves a child, controller task, proxy listener,
  owned proxy connection, or profile behind;
- the locked dependency audit no longer completes with zero findings/warnings.

Preserve relevant redacted logs and a minimal local reproduction for the private
report; do not continue operating by disabling the affected boundary.
