# TEMPLATE for the Homebrew formula. The LIVE formula is
# Formula/doover.rb in the public tap repo (CaydenChik/homebrew-doover);
# on each release, update that copy with the new version and the four
# sha256 values from the SHA256SUMS asset of the tagged GitHub release
# (see LAUNCH.md step 4). This file only tracks the intended shape.
#
#   brew tap caydenchik/doover
#   brew install doover
class Doover < Formula
  desc "Undo for AI agent shell commands"
  homepage "https://github.com/CaydenChik/doover"
  version "0.2.1"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/CaydenChik/doover/releases/download/v#{version}/doover-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "FILL_FROM_SHA256SUMS_AT_RELEASE"
    else
      url "https://github.com/CaydenChik/doover/releases/download/v#{version}/doover-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "FILL_FROM_SHA256SUMS_AT_RELEASE"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/CaydenChik/doover/releases/download/v#{version}/doover-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "FILL_FROM_SHA256SUMS_AT_RELEASE"
    else
      url "https://github.com/CaydenChik/doover/releases/download/v#{version}/doover-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "FILL_FROM_SHA256SUMS_AT_RELEASE"
    end
  end

  def install
    bin.install "doover"
  end

  def caveats
    <<~EOS
      Wire doover into Claude Code, then check the install:
        doover init
        doover doctor
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/doover --version")
  end
end
