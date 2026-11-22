---
title: Releasing
nav_order: 9
---

# Releasing

vinoSearch uses consecutive integer release numbers (`v1`, `v2`, ...) rather than semantic versioning: each release snapshots `main` at that commit, and the number simply orders releases chronologically.

1. Bump the crate versions to `N.0.0`, and add a `<release version="N.0.0" date="...">` entry at the top of `<releases>` in `packaging/flatpak/dev.vinosearch.VinoSearch.metainfo.xml` with a brief summary of changes. Software centers and vinoPilot read the Flatpak version from this file, and `release.yml` aborts before uploading if the entry is missing. Merge this PR to `main`.
2. Tag the merge commit and publish the release with GitHub's generated notes:

   ```bash
   git tag vN <merge commit> && git push origin vN
   gh release create vN --verify-tag --generate-notes
   ```

A binary reports `vinosearch <number>` only when built by `release.yml`, which sets `VINOSEARCH_VERSION` from the crate version and fails if the tag and crate versions disagree (the Flatpak job injects the same environment variable into the manifest on the runner). All other builds (including `cargo install` and local Flatpak builds) report `vinosearch dev` and never prompt for self-updates.

Publishing triggers [`release.yml`](https://github.com/karanshukla/vinoSearch/blob/main/.github/workflows/release.yml), which uploads `vinosearch-x86_64-linux.tar.gz` and its `.sha256` checksum, then builds and attaches `vinosearch-x86_64.flatpak` and its `.sha256` roughly 20 minutes later.

## Documentation site

[`pages.yml`](https://github.com/karanshukla/vinoSearch/blob/main/.github/workflows/pages.yml) builds `docs/` with Jekyll (`just-the-docs`) and deploys it to `vinosearch.vinopilot.ca` on every push to `main` affecting `docs/`. Pull requests run the build check without deploying.
