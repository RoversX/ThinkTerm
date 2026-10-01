# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately. Do not open a public issue,
discussion, or pull request for them.

Use GitHub's private vulnerability reporting: open the repository's
**Security** tab and choose **Report a vulnerability**, or go directly to
<https://github.com/RoversX/ThinkTerm/security/advisories/new>.

Include as much of the following as you can:

- the ThinkTerm version (`thinkterm -V`) and your operating system;
- the part involved, such as the desktop app, the mux server, browser access,
  SSH and Mosh connections, the plugin server, or the mobile apps;
- steps to reproduce it, or a proof of concept;
- what an attacker can do, and what they need first.

## What happens next

You should get an acknowledgement within a week. ThinkTerm is a small
project, so a fix for a complex issue can take longer; you will hear how it is
going. When the fix is released, the advisory is published and credits you,
unless you would rather not be named. Please keep the details private until
then.

## Supported versions

Security fixes go into the main branch and the next release. Only the latest
release is supported, so please check that a problem still occurs there before
reporting it.

## Scope

The code in this repository is in scope, including the code ThinkTerm
inherits from WezTerm. If an issue also affects WezTerm, say so in your report
so that it can be reported upstream as well.

These are not vulnerabilities in ThinkTerm:

- What an installed plugin can do. A plugin is a program you installed, and it
  runs as you, with your permissions; see the
  [plugin guide](docs/thinkterm/plugins.md#security).
- Access by someone who already holds a valid browser access token, can read
  your ThinkTerm data directory, or can log in as you. Access tokens and the
  key that protects saved SSH passwords are credentials, as
  [Configuration and privacy](README.md#configuration-and-privacy) explains.
- Problems in software that ThinkTerm only runs, such as your shell, `ssh`, or
  `mosh`.

For how ThinkTerm handles your data, see [PRIVACY.md](PRIVACY.md).
