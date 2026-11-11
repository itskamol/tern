# Cask template: scripts/release.sh fills @REPO@ (from Cargo.toml's repository), @VERSION@ and
# @SHA256@ and writes the result to target/release/bundle/tern.rb. Copy that into the tap
# (<owner>/homebrew-tern, Casks/tern.rb); then `brew install --cask <owner>/tern/tern`.
cask "tern" do
  version "@VERSION@"
  sha256 "@SHA256@"

  url "https://github.com/@REPO@/releases/download/v#{version}/tern-#{version}-macos-arm64.zip"
  name "tern"
  desc "Fast, low-memory SSH terminal"
  homepage "https://github.com/@REPO@"

  livecheck do
    url :url
    strategy :github_latest
  end

  # tern downloads and installs its own updates (signed like the installed app), so Homebrew
  # does not race it.
  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :big_sur

  app "tern.app"

  # Built and signed on its maintainer's Mac with a local certificate, not notarised: without
  # this, Gatekeeper refuses to open the downloaded app.
  installed = "#{appdir}/tern.app"
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", installed]
  end

  zap trash: [
    "~/Library/Application Support/tern",
    "~/Library/Logs/tern",
  ]
end
