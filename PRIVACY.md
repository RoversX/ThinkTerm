# Privacy Policy for ThinkTerm

No data about your device(s) or ThinkTerm usage is collected by the ThinkTerm
project.

## Data Maintained by ThinkTerm

ThinkTerm maintains some historical data, such as recent searches or action
usage, in some of its overlays such as the debug overlay and character
selector, in order to make your usage more convenient. It is used only
by the local process, and care is taken to limit access for the associated
files on disk to only your local user identity.

ThinkTerm tracks the output from the commands that you have executed in
a scrollback buffer. At the time of writing, that scrollback buffer
is an in-memory structure that is not visible to other users of the machine.
In the future, if ThinkTerm expands to offload scrollback information to
your local disk, it will do so in such a way that other users on the
same system will not be able to inspect it.

## macOS and Data permissions

On macOS, when a GUI application that has a "bundle" launches child processes
(eg: ThinkTerm, running your shell, and your shell running the programs which
you direct it to run), any permissioned resource access that may be attempted
by those child processes will be reported as though ThinkTerm is attempting to
access those resources.

The result is that from time to time you may see a dialog about ThinkTerm
accessing your Contacts if you run a `find` command that happens to step through
the portion of your filesystem where the contacts are stored. Or perhaps you
are running a utility that accesses your camera; it will appear as though
ThinkTerm is accessing those resources, but it is not: there is no logic within
ThinkTerm to attempt to access your contacts, camera or any other sensitive
information.

## Update Checking

By default, once every 24 hours, ThinkTerm makes an HTTP request to GitHub's
release API in order to determine if a newer version is available and to
notify you if that is the case.

The content of that request is private between your machine and GitHub. The
contributors to ThinkTerm cannot see inside that request and therefore cannot
infer any information from it.

If you wish, you can disable update checking by setting
`check_for_updates = false`.

## Third-Party Builds

The above is true of the ThinkTerm source code and the binaries produced by
ThinkTerm's release workflow and made available from
https://github.com/RoversX/thinkterm/.

If you obtained a pre-built ThinkTerm binary from some other source, be aware
that the person(s) building those versions may have modified them to behave
differently from the source version.
