# Install via an explicitly created local reviewed tap (not an asserted public tap).
# Copy the reviewed distribution directory beside the tap's Formula directory; see README.md.
# No remote installer source is claimed before these distribution sources are published.
class Fabric < Formula
  desc "Trusted-local agent workspaces (reviewed native release)"
  homepage "https://github.com/satansdeer/fabric"
  # Do not let Homebrew unpack the archive before the shared installer validates it.
  url "https://github.com/satansdeer/fabric/releases/download/v0.1.2/fabric-v0.1.2-macos-arm64.tar.gz", using: :nounzip
  version "0.1.2"
  sha256 "81f06df7a50f0e513fb64e409286f06835b0cf29f00eb991c1bfb42f671ec3d6"

  depends_on :macos
  depends_on arch: :arm64
  env :std

  def install
    source = Pathname.new(__dir__).parent/"distribution"
    odie "Distribution source must be an absolute canonical directory" unless source.absolute? && source.directory? && source.realpath == source
    %w[install.rb fabric-v0.1.2-macos-arm64.json fabric-v0.1.2-macos-arm64.tar.gz.sha256].each do |name|
      file = source/name
      odie "Missing or unsafe reviewed distribution source: #{name}" unless file.file? && !file.symlink? && file.realpath == file
    end
    require "json"
    inventory = JSON.parse((source/"fabric-v0.1.2-macos-arm64.json").read)
    odie "Distribution inventory does not match the reviewed public source" unless inventory["version"] == "0.1.2" && inventory["platform"] == "macos-arm64" && inventory["source_sha"] == "14f2d44b40f8489d38537e31eda41dd8752463df"
    expected = "81f06df7a50f0e513fb64e409286f06835b0cf29f00eb991c1bfb42f671ec3d6"
    odie "Distribution checksum pin changed; review formula and public pins together" unless (source/"fabric-v0.1.2-macos-arm64.tar.gz.sha256").read == "#{expected}  fabric-v0.1.2-macos-arm64.tar.gz\n"
    require "digest"
    archive = cached_download.realpath
    odie "Archive checksum mismatch; refusing to parse or extract" unless Digest::SHA256.file(archive).hexdigest == expected
    # The installer enforces the complete public inventory before exposing bin.
    # Deliberately omit --trust-plugins: installation does not execute downloaded code.
    prefix.mkpath
    system "/usr/bin/ruby", source/"install.rb", "--version", "0.1.2", "--platform", "macos-arm64",
           "--prefix", prefix.realpath/"libexec", "--archive", archive
    bin.install_symlink libexec/"bin/fabric"
  end

  def caveats
    <<~EOS
      Installed the native v0.1.2 release from source 14f2d44b40f8489d38537e31eda41dd8752463df.
      Plugins are not registered or executed by installation; there is no implicit trust consent.
      No Developer ID signing/notarization or public Homebrew tap is claimed.
      This local formula requires the reviewed distribution directory beside Formula.
      Future public releases require a reviewed update of the shared installer, pins and formula.
    EOS
  end

  test do
    assert_match "fabric 0.1.2", shell_output("#{bin}/fabric --version")
  end
end
