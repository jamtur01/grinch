# Security policy

## Supported versions

Only the [latest published release](https://github.com/jamtur01/grinch/releases/latest)
is supported for bug reports and security fixes. Older versions are unsupported;
fixes are shipped in new releases and are not backported. Upgrade to the latest
release and check whether the issue still occurs before reporting it.

## Reporting a vulnerability

Report suspected vulnerabilities privately through GitHub's
[Report a vulnerability](https://github.com/jamtur01/grinch/security/advisories/new)
form. Do not disclose vulnerabilities in public issues or pull requests.

Include:

- Grinch version, macOS version, and affected browser/version.
- A description of the impact and any conditions required to trigger it.
- Reproduction steps and a minimal config or proof of concept.
- Relevant diagnostic events, with credentials and personal data removed.

Use synthetic URLs where possible. Diagnostic logs can contain complete URLs,
query parameters, authentication tokens, app names, and local paths. Do not
attach an unredacted log or real sign-in link.

The maintainer will use the private report thread to investigate and coordinate
a fix and disclosure. Keep exploit details private while that work is in progress.

## Configuration and trust

Grinch evaluates JavaScript configuration and uses it to choose applications,
rewrite URLs, and supply browser arguments. Only install configs you trust.
If an incoming URL can override those decisions or inject browser arguments,
report it privately.

For ordinary routing problems and feature requests, follow
[CONTRIBUTING.md](CONTRIBUTING.md).
