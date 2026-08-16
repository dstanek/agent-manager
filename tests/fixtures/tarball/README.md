# Tarball Feature fixture

`tarball-only.tgz` is the packed form of `tarball-only/`: a Feature referenced by URL rather
than pulled from a registry. It is committed rather than built by the test harness because the
thing under test is an **HTTPS fetch**, and `am` refuses any other scheme — a Feature runs as
root while the image is built, so a channel with no integrity guarantee is not on offer. A
locally served HTTPS endpoint does not help either: `ureq` verifies against a bundled Mozilla
root store, which no self-signed certificate can join.

So the fixture is served the one way that needs no infrastructure at all — as a file in this
repository, over GitHub's own HTTPS:

```
https://raw.githubusercontent.com/dstanek/agent-manager/main/tests/fixtures/tarball/tarball-only.tgz
```

The reference CLI can fetch that URL too, which is what makes a differential test possible here
at all.

To rebuild it after editing the sources (the flags keep it byte-identical, so an unchanged
Feature does not churn the repository):

```sh
tar --sort=name --mtime='UTC 2020-01-01' --owner=0 --group=0 --numeric-owner \
    -czf tests/fixtures/tarball/tarball-only.tgz \
    -C tests/fixtures/tarball/tarball-only devcontainer-feature.json install.sh
```

The tests that use it are `#[ignore]`d and read `AM_TARBALL_FIXTURE_URL` when set, so a branch
that changes the fixture can point them at its own ref before it lands on `main`.
