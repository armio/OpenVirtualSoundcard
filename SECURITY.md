# Security policy

## Reporting a vulnerability

Please **don't open a public issue** for a security problem. Report it
privately through GitHub instead: on the repository's **Security** tab,
choose **Report a vulnerability**. Only the maintainers see the report.

Include what you can of:

* the commit or version, and the platform (macOS version, Mac model, or
  Linux distribution);
* what an attacker can do, and from where (the local network, a local
  user, an application);
* steps or a packet capture that reproduce it.

OpenVirtualSoundcard is maintained by volunteers. We aim to acknowledge a
report within a week and to agree on a disclosure date with you. There is
no bug bounty.

## Supported versions

There are no releases yet. Fixes land on `main`.

## What is security-sensitive

* **The macOS daemon runs as root** (a LaunchDaemon) and parses traffic
  from the local network: mDNS, ARC, CMC, conmon, flow control, PTP and
  audio packets. A crash, a hang or memory corruption caused by a packet is
  a vulnerability.
* **The control socket** (`/var/run/ovsc/control.sock`, owned by root and
  the `admin` group) changes settings and restarts the daemon. Anything
  that lets a non-administrator use it, or makes the daemon write outside
  its configuration and state files, is a vulnerability.
* **The Core Audio driver** runs inside Core Audio's helper process and
  shares memory with the daemon. Anything that lets another process reach
  that memory, or makes the driver misbehave from it, is a vulnerability.

Not a vulnerability in OpenVirtualSoundcard: Dante networks have no
authentication, so any device on the network can route, rename and
reconfigure any other. That is how the protocol works. Run audio networks
on a separate, trusted network or VLAN.
