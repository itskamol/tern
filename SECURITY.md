# Security policy

tern handles SSH credentials and host keys, so security reports get priority.

## Reporting a vulnerability

Report privately through GitHub: **Security → Report a vulnerability** on this repository, or open a [new advisory](https://github.com/itskamol/tern/security/advisories/new). Do not open a public issue.

Include what an attacker can do, the steps or code to reproduce it, and the commit you tested. You will get an acknowledgement within 7 days and a fix or a plan within 30.

## Supported versions

tern has no release yet. Until 1.0, only the latest commit on `main` receives fixes.

## Known accepted risks

| Advisory | Status |
| --- | --- |
| [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071) (Marvin timing side channel in `rsa`, via russh) | No fixed release exists. Accepted for now: tern performs at most one RSA signature per login and host-key checks use only public keys. Decision tracked in [#16](https://github.com/your-moon/tern/issues/16). |
