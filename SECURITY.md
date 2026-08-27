# Security policy

yesnodb is pre-release and has no supported production release. The project has
not been operated at production scale. Do not use it for important data without
an independent review of durability, authentication, backup, and recovery.

## Supported versions

Until the first tagged release, only the current `main` branch receives security
fixes. There is no backport policy.

## Reporting a vulnerability

Do not include exploit details, credentials, certificates, database images, or
other sensitive material in a public issue.

Use GitHub's private vulnerability-reporting or draft security-advisory flow for
the `moriyoshi/yesnodb` repository. If that private flow is unavailable, contact
the repository owner privately through the contact method on their GitHub
profile before disclosing details.

Include:

- the affected revision and platform;
- the reachable interface and required privileges;
- a minimal reproduction;
- expected and observed behavior;
- likely impact;
- whether the issue affects confidentiality, integrity, availability, or
  durability.

The project will acknowledge receipt through the same private channel. Because
there is no release or response-time commitment yet, no remediation SLA is
promised.

## Deployment-sensitive surfaces

The replication listener can transfer complete database images. Never expose it
without mutual TLS and a `replica` principal.

A non-loopback Flight listener should use TLS and explicit principals. The
`--insecure` and `--insecure-replication` flags are deliberate operator
overrides, not recommended defaults.

Replication is not a backup, failover is manual, and an unfenced old leader can
continue accepting writes. Follow the operations guide for backup, promotion,
leadership-term fencing, and restore.
