---
title: Releasing
nav_order: 9
---

# Releasing

Versions are consecutive numbers (`v1`, `v2`, ...), not semver: a release is main at that point, and
the number only says which one is newer.

1. Bump the crates to `N.0.0`, and add a `<release version="N.0.0" date="...">` at the top of
   `packaging/flatpak/dev.vinosearch.VinoSearch.metainfo.xml`'s `<releases>`, with a line or two
   on what changed. Software centres and vinoPilot read the Flatpak's version from it, and
   `release.yml` stops before uploading anything when it's missing. Merge that.
2. Tag the merge commit and publish with GitHub's generated notes:

   ```bash
   git tag vN <merge commit> && git push origin vN
   gh release create vN --verify-tag --generate-notes
   ```

A binary reports `vinosearch <number>` only when `release.yml` built it, which sets
`VINOSEARCH_VERSION` to the crates' version (the build fails if the two differ); the Flatpak job
adds it to the manifest's build env on the runner. Every other build, a `cargo install` or a local
Flatpak build included, reports `vinosearch dev`, and is never offered an update.

Publishing runs [`release.yml`](https://github.com/karanshukla/vinoSearch/blob/main/.github/workflows/release.yml),
which attaches `vinosearch-x86_64-linux.tar.gz` and its `.sha256`, then builds the Flatpak and
attaches `vinosearch-x86_64.flatpak` and its `.sha256` about 20 minutes later.

## This site

[`pages.yml`](https://github.com/karanshukla/vinoSearch/blob/main/.github/workflows/pages.yml) builds
`docs/` with Jekyll (just-the-docs) and publishes it to vinosearch.vinopilot.ca on every push to main
that touches `docs/`. Pull requests build it without deploying.
