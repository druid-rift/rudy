# Security policy

Rudy erases and partitions disks, so a flaw in how it chooses or authorises a target is a
security problem even when no attacker is involved.

## Reporting a vulnerability

Report it privately through **Report a vulnerability** on this repository's Security tab.
Do not open a public issue, and do not include a working exploit in one.

Say which version (`rudy --version`), what you did, and what was written or exposed that
should not have been.

## What counts

- Rudy writing to, or offering to write to, a disk its safety policy should refuse: a
  system disk, or a target whose evidence is missing.
- Anything that widens the Flatpak sandbox's reach beyond what
  `packaging/flatpak/dev.rudy.Rudy.yml` justifies line by line.
- The boot payload running or trusting something it should not.

Secure Boot is not supported and its absence is not a vulnerability.

## Supported versions

Only the latest release receives fixes. Rudy is pre-release.
