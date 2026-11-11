# Releasing tern

Releases are cut from `main` by pushing a `vX.Y.Z` tag. The [release workflow](.github/workflows/release.yml) refuses a tag that does not match the workspace version or whose changelog section is missing.

1. Check that `main` is green in CI and that every issue in the milestone is closed.
2. Pick the version per [SemVer](https://semver.org/): before 1.0, bump the minor for anything breaking or notable, the patch for fixes.
3. Set `version` under `[workspace.package]` in `Cargo.toml`, and the same version on the four `tern-*` entries under `[workspace.dependencies]`, then run `cargo check --workspace` so `Cargo.lock` follows.
4. In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a fresh empty `## [Unreleased]` above it, and update the compare links at the bottom.
5. Record the release's idle RSS in `docs/perf.md`; a regression of more than 10% blocks the release.
6. Commit as `Release vX.Y.Z`, then tag and push:

   ```sh
   git tag -a vX.Y.Z -m "tern vX.Y.Z"
   git push origin main vX.Y.Z
   ```

7. The workflow runs the gates again and publishes a GitHub release whose notes are that version's changelog section.

The signed macOS app bundle is attached by the workflow once the app crate exists (#10); until then releases carry notes only.

## The macOS download (dmg and zip)

`scripts/release.sh` builds `target/release/bundle/tern-X.Y.Z-macos-arm64.dmg` (drag tern to Applications) and the `.zip` the Homebrew cask uses, and prints the `gh release create` command without running it. For a build that opens without a Gatekeeper warning, run it on the Mac that holds the Developer ID certificate:

```sh
xcrun notarytool store-credentials tern-notary   # once: Apple ID, team ID, app-specific password
TERN_SIGN_ID="Developer ID Application: Enkhjil Erdenebat (X6M62VN277)" \
TERN_NOTARY_PROFILE=tern-notary scripts/release.sh
```

It signs and notarises the app, staples it, builds the dmg from the stapled app, signs, notarises and staples the dmg, then checks both with `spctl`. Without `TERN_NOTARY_PROFILE` it still signs and skips notarisation, saying so.
