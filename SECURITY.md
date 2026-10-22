# Security policy

## Supported versions

Only the latest release on the default branch receives security fixes.

## Reporting a vulnerability

Please **do not** open a public issue for a suspected vulnerability.
Report it privately through GitHub's advisory form:

https://github.com/hellpuffyt/ratelimit-rs/security/advisories/new

Include the version or commit, a description of the issue, and steps or
input that reproduce it. You should receive an acknowledgement within a
few days; once a fix is available it will be released and credited in
the advisory unless you prefer otherwise.

## Scope

ratelimit-rs is a dependency-free library. Relevant reports include a limiter admitting more than its documented bound, arithmetic overflow or panics reachable through the public API, and unbounded memory growth beyond what `Keyed` documents.
