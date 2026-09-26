# Getting help

Zygo has one maintainer and no company behind it, so the fastest route to an
answer is the one that leaves a trace others can find.

| You want to… | Do this |
|---|---|
| find out how something works | read [the book](docs/book/README.md); [chapter 22](docs/book/22-troubleshooting.md) has the errors people hit and the fix for each |
| check your host | run `zygo doctor`; it prints the fix for each thing it finds |
| report a bug | open an issue with the *Bug report* form, with `zygo doctor` output and `uname -r` |
| ask a question or propose something | open an issue with the *Feature request* form |
| report a sandbox escape or a leak | **not an issue**: follow [SECURITY.md](SECURITY.md), privately |

What helps a bug report most, in order: the exact command, the exact output,
the kernel (`uname -r`), the distribution, whether it runs on a plain host,
in a container or in the Mac VM, and `zygo doctor`'s report. `zygo run
--dry-run --json` prints the mount plan and the limits without running
anything, and is often the whole answer.

There is no chat channel and no paid support. Issues are read within a few
days; security reports within the times [SECURITY.md](SECURITY.md) states.
