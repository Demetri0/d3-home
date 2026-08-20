# Homebrew formula. Lives here for reference; the copy that matters is the
# one in a tap, or in homebrew-core once the project is old enough to be
# accepted there.
class D3home < Formula
  desc "Command line for smart home devices on the local network"
  homepage "https://github.com/Demetri0/d3-home"
  url "https://github.com/Demetri0/d3-home/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/Demetri0/d3-home.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/d3home")
    man1.install "packaging/d3home.1"
    generate_completions_from_executable(bin/"d3home", "completions", shell_parameter_format: :arg)
    doc.install "README.md", "docs/protocol.md"
    pkgshare.install "contrib"
  end

  def caveats
    <<~EOS
      For notifications that carry d3home's own icon rather than Terminal's,
      build the application bundle: macOS shows the icon of whichever
      application called it, and a command line program has none.

        packaging/macos/bundle.sh

      To run the watcher at login, see the LaunchAgent in
        #{pkgshare}/contrib/com.d3home.daemon.plist
    EOS
  end

  test do
    assert_match "d3home", shell_output("#{bin}/d3home --version")
    # No device is configured in a sandbox, so this is the expected failure
    # rather than a broken build: exit 2 is the usage/config code.
    assert_match "no device registry", shell_output("#{bin}/d3home devices 2>&1", 2)
  end
end
