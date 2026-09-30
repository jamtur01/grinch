---
name: Bug report
about: Report a problem with the latest Grinch release
title: ''
labels: ''
assignees: ''

---

Only the [latest published release](https://github.com/jamtur01/grinch/releases/latest)
is supported. Upgrade and reproduce the problem before filing; older releases
do not receive backported fixes.

For suspected vulnerabilities, use
[private reporting](https://github.com/jamtur01/grinch/security/advisories/new)
instead. See the [security policy](https://github.com/jamtur01/grinch/blob/main/SECURITY.md).

- [ ] I reproduced this with the latest published Grinch release.

**Environment**

- Grinch version (menu bar or `Grinch --version`):
- macOS version and Apple Silicon/Intel:
- Destination browser and version:
- Originating app, if relevant:

**Describe the bug**

What happened, and what did you expect?

**Reproduction**

Include steps, a minimal config, and a sanitized example URL. Mention profile
selection or modifier keys if they affect routing.

**Diagnostics**

Include relevant events from **Open Diagnostic Log** and, where useful,
`Grinch --test "<example-url>"` output. Enable `options.logRequests` for routing
decisions; launch failures are logged even when that option is off.

Remove credentials, sign-in links, personal data, and local paths before posting.
