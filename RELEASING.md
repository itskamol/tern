# Releasing tern

Releases are cut from `main` on itskamol/tern, pushed to the `fork` remote (`origin` is upstream). The [release workflow](.github/workflows/release.yml) runs on the `vX.Y.Z` tag, refuses one that does not match the workspace version or whose changelog section is missing, runs the gates again and sets the release notes to that version's changelog section.

1. Check that `main` is green in CI and that every issue in the milestone is closed.
2. Pick the version per [SemVer](https://semver.org/): before 1.0, bump the minor for anything breaking or notable, the patch for fixes.
3. Set `version` under `[workspace.package]` in `Cargo.toml`, and the same version on the four `tern-*` entries under `[workspace.dependencies]`, then run `cargo check --workspace` so `Cargo.lock` follows.
4. In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add a fresh empty `## [Unreleased]` above it, and update the compare links at the bottom.
5. Check the release's memory: `scripts/bundle-app.sh`, then `scripts/measure-memory.sh` with the screen unlocked and awake and at least half of the copy's window showing (it refuses to read otherwise, and says why). It measures the scenes in `docs/perf.md`, each the median of three runs, and exits non-zero when a median is over its ceiling there. A failing check blocks the release. Add the row it prints to `docs/perf.md`; compare with the previous release's row, taken the same way on the same Mac.
6. Commit as `Release vX.Y.Z` and push: `git push fork main`.
7. Build the download with `scripts/release.sh` (signing: [below](#signing)). It writes `target/release/bundle/tern-X.Y.Z-macos-arm64.{zip,dmg}` and the Homebrew cask `tern.rb`, and prints the `gh release create` command without running it.
8. Publish the release with its files in one step, which also creates the tag; the workflow then only sets the notes:

   ```sh
   gh release create vX.Y.Z target/release/bundle/tern-X.Y.Z-macos-arm64.{zip,dmg} --repo itskamol/tern \
     --target main --title "tern vX.Y.Z" --notes-file <(awk '/^## \[X.Y.Z\]/{f=1;next} /^## \[/{f=0} f' CHANGELOG.md)
   ```

9. Copy `target/release/bundle/tern.rb` to the tap repository `itskamol/homebrew-tern` as `Casks/tern.rb`, commit and push.

## Signing

**With the Developer ID certificate** the build opens without a Gatekeeper warning. Run step 7 on the Mac that holds it:

```sh
xcrun notarytool store-credentials tern-notary   # once: Apple ID, team ID, app-specific password
TERN_SIGN_ID="Developer ID Application: Enkhjil Erdenebat (X6M62VN277)" \
TERN_NOTARY_PROFILE=tern-notary scripts/release.sh
```

It signs and notarises the app, staples it, builds the dmg from the stapled app, signs, notarises and staples the dmg, then checks both with `spctl`. Without `TERN_NOTARY_PROFILE` it still signs and skips notarisation, saying so.

**Without it**, `scripts/release.sh` signs with the local identity "tern local" (create it once with `scripts/make-local-identity.sh`). Such a release opens because the Homebrew cask removes the quarantine flag, and it updates itself: an installed tern accepts a download only when it is signed with the same certificate. So keep that identity: export "tern local" from Keychain Access (My Certificates → Export, a `.p12` with a password) and store it safely; a release signed with a new one is refused by every installed copy, which then needs `brew reinstall --cask tern`.
