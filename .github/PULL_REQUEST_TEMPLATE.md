<!-- Security fixes: talk to the maintainer through SECURITY.md before opening a public pull request. -->

## What and why

<!-- What changes, and the reason for it. Link the issue if there is one. -->

## How it was checked

<!-- Which suites ran, and on which kernel (`uname -r`). For changes to the launcher,
     seccomp, Landlock, mounts or the network: `make verify-linux escape-linux fuzz-linux`. -->

- [ ] `make test` and `make lint` pass
- [ ] The book is updated in this change, or nothing a user can see changed
- [ ] A line under *Unreleased* in `CHANGELOG.md`, if a user would notice
