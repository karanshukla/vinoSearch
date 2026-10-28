---
title: Releasing
nav_order: 9
---

# Releasing

Versions are consecutive numbers (`v1`, `v2`, ...), not semver: a release is main at that point, and
the number only says which one is newer.

1. Bump the crates to `N.0.0` in a PR and merge it.
2. Tag the merge commit and publish with GitHub's generated notes:

   ```bash
   git tag vN <merge commit> && git push origin vN
   gh release create vN --verify-tag --generate-notes
   ```

Publishing runs [`release.yml`](https://github.com/karanshukla/vinoSearch/blob/main/.github/workflows/release.yml),
which attaches `vinosearch-x86_64-linux.tar.gz` and its `.sha256`.

## This site

[`pages.yml`](https://github.com/karanshukla/vinoSearch/blob/main/.github/workflows/pages.yml) builds
`docs/` with Jekyll (just-the-docs) and publishes it to vinosearch.vinopilot.ca on every push to main
that touches `docs/`. Pull requests build it without deploying.
