#!/usr/bin/ruby
# Node-free, pinned macOS arm64 installer. Requiring this file never installs or executes anything.
require 'digest'
require 'json'
require 'zlib'
require 'stringio'
require 'fileutils'
require 'tmpdir'
require 'optparse'
require 'net/http'
require 'uri'
require 'openssl'
require 'fiddle'
require 'open3'
require 'timeout'

module FabricDistribution
  class Error < StandardError; end
  VERSION = '0.1.2'.freeze
  PLATFORM = 'macos-arm64'.freeze
  SOURCE = '14f2d44b40f8489d38537e31eda41dd8752463df'.freeze
  ROOT = "fabric-v#{VERSION}-#{PLATFORM}".freeze
  ARCHIVE = "#{ROOT}.tar.gz".freeze
  URL = "https://github.com/satansdeer/fabric/releases/download/v#{VERSION}/#{ARCHIVE}".freeze
  MAX_ARCHIVE = 32 * 1024 * 1024
  MAX_EXPANDED = 64 * 1024 * 1024
  MAX_ENTRIES = 128
  PAYLOADS = %w[bin/fabric bin/fabric-fake-harness bin/fabric-local-target
    plugins/omp/package.json plugins/omp/plugin plugins/codex/package.json plugins/codex/plugin
    plugins/local/package.json plugins/local/plugin sdk/cli.mjs sdk/package.mjs sdk/protocol.mjs
    sdk/conformance.mjs sdk/index.mjs integrations/omp/fabric.ts integrations/omp/host-smoke.ts
    scripts/smoke-release.mjs].freeze
  EXECUTABLES = %w[bin/fabric bin/fabric-fake-harness bin/fabric-local-target
    plugins/omp/plugin plugins/codex/plugin plugins/local/plugin].freeze
  NATIVE = {
    'omp' => { 'executable' => 'omp', 'root_environment' => ['PI_CODING_AGENT_DIR'],
      'fingerprint' => { 'domain' => 'omp-native-config-v1',
        'files' => %w[config.yml config.yaml settings.json models.yml models.yaml mcp.json .mcp.json],
        'roots' => [['native-agent-dir', ''], ['native-home', '.omp/agent']] } },
    'codex' => { 'executable' => 'codex', 'root_environment' => ['CODEX_HOME'],
      'fingerprint' => { 'domain' => 'codex-native-config-v1', 'files' => ['config.toml'],
        'roots' => [['native-config-dir', '']] } }
  }.freeze

  module_function

  def check(condition, message)
    raise Error, message unless condition
  end

  def hash(bytes)
    Digest::SHA256.hexdigest(bytes)
  end

  def decode(bytes)
    text = bytes.dup.force_encoding(Encoding::UTF_8)
    check(text.valid_encoding?, 'invalid UTF-8 JSON')
    JSON.parse(text)
  rescue JSON::ParserError => e
    raise Error, "invalid JSON: #{e.message}"
  end

  def safe_path?(name)
    name.is_a?(String) && name.bytesize <= 240 &&
      name.split('/', -1).all? { |part| /\A[A-Za-z0-9_@.-]+\z/.match(part) && part != '.' && part != '..' }
  end

  def read_file(path, limit)
    File.open(path, File::RDONLY | File::NOFOLLOW | File::NONBLOCK) do |file|
      check(file.stat.file? && file.stat.size <= limit, "not a bounded regular file: #{path}")
      bytes = file.read(limit + 1) || ''.b
      check(bytes.bytesize <= limit, "file exceeds size limit: #{path}")
      bytes
    end
  end

  def pins
    inventory = read_file(File.join(__dir__, "#{ROOT}.json"), 65_536)
    manifest = decode(inventory)
    validate_manifest(manifest)
    checksum = read_file(File.join(__dir__, "#{ARCHIVE}.sha256"), 256)
    match = /\A([a-f0-9]{64})  #{Regexp.escape(ARCHIVE)}\n\z/.match(checksum)
    check(match, 'invalid trusted archive checksum file')
    [inventory, manifest, match[1]]
  end

  def validate_manifest(m)
    check(m.is_a?(Hash), 'release inventory must be an object')
    expected = { 'version' => VERSION, 'tag' => "v#{VERSION}", 'platform' => PLATFORM,
      'source_sha' => SOURCE, 'build_kind' => 'release', 'archive' => ARCHIVE,
      'cli_version' => "fabric #{VERSION} (source #{SOURCE}; #{PLATFORM}; release)",
      'source_url' => "https://github.com/satansdeer/fabric/tree/#{SOURCE}" }
    expected.each { |key, value| check(m[key] == value, "release provenance mismatch: #{key}") }
    check(m['files'].is_a?(Hash) && m['files'].keys.sort == PAYLOADS.sort &&
      m['files'].values.all? { |h| h.is_a?(String) && /\A[a-f0-9]{64}\z/.match(h) }, 'incomplete release inventory')
    check(m['build'].is_a?(Hash) && m['build']['cargo_lock_sha256'] ==
      '118ca68f1feb1818874b3f0ad5bb4a01b6b76b848879f15e4435e4cab6e461a2' &&
      m['build']['rustc'] == 'rustc 1.91.0 (f8297e351 2025-10-28)' &&
      m['build']['cargo'] == 'cargo 1.91.0 (ea2d97820 2025-10-10)', 'release build provenance mismatch')
    groups = [m['plugins'], m['target_plugins']]
    check(groups.all? { |g| g.is_a?(Array) } && groups[0].map { |p| p['id'] } == %w[omp codex] &&
      groups[1].map { |p| p['id'] } == ['local'], 'official plugin IDs mismatch')
    groups.flatten.each do |p|
      check(p['version'] == '0.1.0' && p['protocol'] == 1 &&
        p['sha256'].is_a?(String) && /\A[a-f0-9]{64}\z/.match(p['sha256']), 'official plugin version/protocol/pin mismatch')
    end
    check(m['plugins'][0]['supported_cli'] == 'omp/19.1.0' &&
      m['plugins'][1]['supported_cli'] == 'codex-cli 0.171.0', 'native CLI version mismatch')
    check(m['omp'] == { 'version' => 'omp/19.1.0',
      'url' => 'https://github.com/can1357/oh-my-pi/releases/download/v19.1.0/omp-darwin-arm64',
      'sha256' => 'dca5643669ed5e553f707bd065499babd0fe3030696bbb24c9da09c798af5abb',
      'bundled' => false }, 'OMP release pin mismatch')
    check(m['omp_extension'] == { 'path' => 'integrations/omp/fabric.ts',
      'sha256' => m['files']['integrations/omp/fabric.ts'],
      'source_url' => "https://github.com/satansdeer/fabric/blob/#{SOURCE}/integrations/omp/fabric.ts" }, 'OMP extension pin mismatch')
    check(m['fixture'] == { 'path' => 'bin/fabric-fake-harness',
      'sha256' => m['files']['bin/fabric-fake-harness'],
      'purpose' => 'provider-free fake-a/fake-b only; not a native model worker' }, 'fixture pin mismatch')
    m
  end

  def tar_string(field)
    head, tail = field.split("\0", 2)
    check(!tail || tail.bytes.all? { |b| b == 0 }, 'unsafe tar string padding')
    head
  end

  def octal(field)
    check(/\A[ 0-7]+[\0 ]*\z/.match(field), 'invalid tar numeric field')
    field.delete("\0 ").to_i(8)
  end

  # Only the released POSIX ustar format is accepted: no PAX/GNU overrides or links.
  # The entire bounded snapshot is validated in memory before any payload is written.
  def parse_tar(bytes)
    files = {}
    seen = {}
    allowed_dirs = { ROOT => true }
    (PAYLOADS + ['release.json']).each do |name|
      parts = "#{ROOT}/#{name}".split('/')
      (1...parts.length).each { |n| allowed_dirs[parts.first(n).join('/')] = true }
    end
    expanded = 0
    gzip = Zlib::GzipReader.new(StringIO.new(bytes))
    read = lambda do |size|
      expanded += size
      check(expanded <= MAX_EXPANDED, 'archive expanded size limit exceeded')
      chunk = gzip.read(size)
      check(chunk && chunk.bytesize == size, 'truncated tar archive')
      chunk
    end
    count = 0
    loop do
      header = read.call(512)
      if header == "\0" * 512
        check(read.call(512) == "\0" * 512, 'invalid tar end marker')
        loop do
          tail = gzip.read(512)
          break if tail.nil? || tail.empty?
          expanded += tail.bytesize
          check(expanded <= MAX_EXPANDED && tail.bytesize == 512 && tail.bytes.all? { |b| b == 0 }, 'unsafe trailing tar data')
        end
        check(gzip.unused.nil? || gzip.unused.empty?, 'trailing or concatenated gzip data')
        break
      end
      count += 1
      check(count <= MAX_ENTRIES, 'archive entry count limit exceeded')
      checksum = header.dup
      checksum[148, 8] = ' ' * 8
      check(octal(header[148, 8]) == checksum.bytes.inject(0, :+), 'tar header checksum mismatch')
      check(header[257, 8] == "ustar\00000", 'unsupported tar format; POSIX ustar required')
      type = header[156, 1]
      check(['0', "\0", '5'].include?(type), 'unsafe tar entry type (links/PAX/devices unsupported)')
      check(tar_string(header[157, 100]).empty?, 'unsafe tar link target')
      name = tar_string(header[0, 100])
      prefix = tar_string(header[345, 155])
      name = "#{prefix}/#{name}" unless prefix.empty?
      directory = type == '5'
      name = name[0...-1] if directory && name.end_with?('/')
      check(safe_path?(name) && (name == ROOT || name.start_with?(ROOT + '/')), 'unsafe tar path')
      alias_name = name.downcase
      check(!seen.key?(alias_name), 'duplicate or case-alias tar entry')
      seen[alias_name] = true
      size = octal(header[124, 12])
      check(size <= MAX_ARCHIVE && (!directory || size == 0), 'invalid tar entry size')
      %w[100 108 116 136].each { |offset| octal(header[offset.to_i, offset == '136' ? 12 : 8]) }
      if directory
        check(allowed_dirs[name], 'unpinned archive directory')
      else
        check(name != ROOT, 'archive root must be a directory')
        relative = name[(ROOT.length + 1)..-1]
        check(PAYLOADS.include?(relative) || relative == 'release.json', 'unpinned archive file')
        files[relative] = read.call(size)
        padding = (512 - size % 512) % 512
        check(read.call(padding).bytes.all? { |b| b == 0 }, 'invalid tar body padding') if padding > 0
      end
    end
    files
  rescue Zlib::Error, Zlib::GzipFile::Error, EOFError => e
    raise Error, "invalid gzip archive: #{e.message}"
  ensure
    gzip.close if gzip
  end

  def content_hash(files)
    hash(files.keys.sort.map { |name| "#{name}\0#{hash(files.fetch(name))}\n" }.join)
  end

  def verify_payload(files, inventory, manifest)
    check(files.keys.sort == (PAYLOADS + ['release.json']).sort, 'archive file set does not match release inventory')
    check(files['release.json'] == inventory, 'embedded release.json differs from trusted inventory')
    validate_manifest(decode(files['release.json']))
    manifest['files'].each { |name, pin| check(hash(files.fetch(name)) == pin, "payload checksum mismatch: #{name}") }
    (manifest['plugins'] + manifest['target_plugins']).each do |plugin|
      id = plugin['id']
      package_files = { 'package.json' => files.fetch("plugins/#{id}/package.json"), 'plugin' => files.fetch("plugins/#{id}/plugin") }
      check(content_hash(package_files) == plugin['sha256'], "plugin content-tree checksum mismatch: #{id}")
      expected = { 'id' => id, 'kind' => id == 'local' ? 'target' : 'harness', 'entry' => 'plugin',
        'sdk' => '0.1.0', 'protocol' => 1, 'runtime' => 'native', 'args' => id == 'local' ? [] : [id] }
      expected['native'] = NATIVE.fetch(id) unless id == 'local'
      check(decode(package_files['package.json']) == { 'name' => id, 'version' => plugin['version'],
        'files' => ['plugin'], 'fabricPlugin' => expected }, "official plugin metadata mismatch: #{id}")
    end
    files
  end

  def verify(bytes, inventory, manifest, archive_hash)
    check(bytes.bytesize <= MAX_ARCHIVE, 'archive size limit exceeded')
    check(hash(bytes) == archive_hash, 'archive checksum mismatch; do not extract or execute')
    verify_payload(parse_tar(bytes), inventory, manifest)
  end

  def download(url, redirects = 5)
    uri = URI(url)
    check(uri.is_a?(URI::HTTPS) && !uri.userinfo, 'download requires HTTPS without credentials')
    body = ''.b
    http = Net::HTTP.new(uri.host, uri.port, nil)
    http.use_ssl = true
    http.verify_mode = OpenSSL::SSL::VERIFY_PEER
    http.open_timeout = 20
    http.read_timeout = 60
    http.start do |connection|
      connection.request(Net::HTTP::Get.new(uri.request_uri, 'Accept-Encoding' => 'identity')) do |response|
        if response.is_a?(Net::HTTPRedirection)
          check(redirects > 0 && response['location'], 'too many or invalid download redirects')
          return download(URI.join(url, response['location']).to_s, redirects - 1)
        end
        check(response.is_a?(Net::HTTPSuccess), "release download failed (HTTP #{response.code}); use --archive with the pinned local archive when offline")
        response.read_body do |chunk|
          check(body.bytesize + chunk.bytesize <= MAX_ARCHIVE, 'download exceeds archive size limit')
          body << chunk
        end
      end
    end
    body
  end

  def validate_prefix(prefix)
    check(prefix.is_a?(String) && prefix.start_with?('/') && File.expand_path(prefix) == prefix &&
      prefix != '/' && !prefix.end_with?('/'), '--prefix must be a new canonical absolute directory')
    parent = File.dirname(prefix)
    check(File.realpath(parent) == parent && File.directory?(parent), 'installation parent must already exist and be canonical (no symlinks)')
    ancestor = parent
    loop do
      stat = File.lstat(ancestor)
      protected = (stat.mode & 0022).zero? || (stat.mode & 01000) != 0
      check(stat.directory? && [0, Process.uid].include?(stat.uid) && protected,
        "installation ancestry must be owned by you or root and protected from other users (no group/other write without sticky protection): #{ancestor}; use a protected private prefix")
      break if ancestor == '/'
      ancestor = File.dirname(ancestor)
    end
    check(!File.exist?(prefix) && !File.symlink?(prefix), 'destination already exists; select a new --prefix (nothing was overwritten)')
    parent
  end

  def publish(stage, prefix)
    # macOS renamex_np(RENAME_EXCL) is atomic and never replaces even an empty directory.
    # Ruby File.rename lacks that guarantee; do not substitute a check-then-rename.
    rename = Fiddle::Function.new(Fiddle::Handle::DEFAULT['renamex_np'],
      [Fiddle::TYPE_VOIDP, Fiddle::TYPE_VOIDP, Fiddle::TYPE_INT], Fiddle::TYPE_INT)
    result = rename.call(stage, prefix, 4)
    check(result == 0, "destination publication refused (errno #{Fiddle.last_error}); select a new --prefix; nothing was overwritten")
  end

  def write_payload(root, files)
    files.each do |name, bytes|
      path = File.join(root, name)
      FileUtils.mkdir_p(File.dirname(path), :mode => 0700)
      File.open(path, File::WRONLY | File::CREAT | File::EXCL, EXECUTABLES.include?(name) ? 0700 : 0600) { |f| f.write(bytes) }
    end
  end

  def host(binary, registry, command, argument = nil)
    argv = [binary, 'plugins', '--registry', registry, command]
    argv << argument if argument
    Open3.popen3({ 'PATH' => '/usr/bin:/bin', 'LC_ALL' => 'C' }, *argv,
      :chdir => '/', :unsetenv_others => true, :pgroup => true) do |input, output, errors, waiter|
      input.close
      readers = [output, errors].map { |io| Thread.new { io.read(1024 * 1024 + 1) || ''.b } }
      begin
        Timeout.timeout(15) do
          result = readers.map(&:value)
          warn "Core plugin #{command} stdout:\n#{result[0]}\nstderr:\n#{result[1]}"
          check(result.all? { |s| s.bytesize <= 1024 * 1024 } && waiter.value.success?,
            "Core plugin #{command} rejected; installation retained at #{File.dirname(File.dirname(binary))}")
        end
      ensure
        Process.kill('KILL', -waiter.pid) if waiter.alive?
        readers.each { |t| t.kill if t.alive? }
      end
    end
  rescue Timeout::Error
    raise Error, 'Core plugin registration timed out; installation retained; inspect the private registry before retrying'
  end

  def register(prefix, files, manifest)
    registry = File.join(prefix, 'registry')
    Dir.mkdir(registry, 0700)
    packages = File.join(registry, 'packages')
    Dir.mkdir(packages, 0700)
    binary = File.join(prefix, 'bin/fabric')
    host(binary, registry, 'list')
    (manifest['plugins'] + manifest['target_plugins']).each do |plugin|
      id = plugin['id']
      destination = File.join(packages, plugin['sha256'])
      Dir.mkdir(destination, 0700)
      %w[package.json plugin].each do |name|
        File.open(File.join(destination, name), File::WRONLY | File::CREAT | File::EXCL,
          name == 'plugin' ? 0700 : 0600) { |f| f.write(files.fetch("plugins/#{id}/#{name}")) }
      end
      kind = id == 'local' ? 'target' : 'harness'
      registration = { 'id' => id, 'version' => plugin['version'], 'protocol' => { 'min' => 1, 'max' => 1 },
        'kind' => kind, 'executable' => File.join(destination, 'plugin'), 'args' => id == 'local' ? [] : [id],
        'capabilities' => [kind == 'target' ? 'target.execute' : 'harness.run'],
        'integrity' => { 'root' => destination, 'sha256' => plugin['sha256'] } }
      registration['native'] = NATIVE.fetch(id) unless id == 'local'
      pending = File.join(registry, ".manifest-#{id}.json")
      begin
        File.open(pending, File::WRONLY | File::CREAT | File::EXCL, 0600) { |f| f.write(JSON.generate(registration)) }
        host(binary, registry, 'install', pending)
      ensure
        File.unlink(pending) if File.file?(pending)
      end
    end
    registry
  end

  def install(version:, platform:, prefix:, archive: nil, trust_plugins: false)
    check(version == VERSION, "unsupported version #{version.inspect}; supported: #{VERSION}")
    check(platform == PLATFORM, "unsupported platform #{platform.inspect}; supported: #{PLATFORM}")
    check(RUBY_PLATFORM.include?('darwin') && /arm64|aarch64/.match(RUBY_PLATFORM), 'this release requires native macOS arm64 system Ruby')
    parent = validate_prefix(prefix)
    inventory, manifest, archive_hash = pins
    if archive
      check(archive.start_with?('/') && File.expand_path(archive) == archive, '--archive must be a canonical absolute local file path')
      check(File.realpath(archive) == archive, '--archive must not traverse symlinks')
    end
    bytes = archive ? read_file(archive, MAX_ARCHIVE) : Timeout.timeout(180) { download(URL) }
    files = verify(bytes, inventory, manifest, archive_hash)
    stage = Dir.mktmpdir('.fabric-install-', parent)
    begin
      write_payload(stage, files)
      validate_prefix(prefix)
      publish(stage, prefix)
      stage = nil
    ensure
      FileUtils.remove_entry_secure(stage) if stage
    end
    if trust_plugins
      warn 'TRUST WARNING: registering reviewed native plugins as your user, not in a sandbox (--trust-plugins consent).'
      register(prefix, files, manifest)
    end
    { 'installed' => prefix, 'version' => VERSION, 'platform' => PLATFORM,
      'source_sha' => SOURCE, 'archive_sha256' => archive_hash,
      'plugins_registered' => trust_plugins, 'registry' => trust_plugins ? File.join(prefix, 'registry') : nil }
  end

  def main(argv)
    options = {}
    parser = OptionParser.new do |o|
      o.banner = 'usage: /usr/bin/ruby install.rb --version 0.1.2 --platform macos-arm64 --prefix NEW_ABSOLUTE_DIRECTORY [--archive ABS_LOCAL_TAR] [--trust-plugins]'
      o.on('--version VERSION') { |v| options[:version] = v }
      o.on('--platform PLATFORM') { |v| options[:platform] = v }
      o.on('--prefix PATH') { |v| options[:prefix] = v }
      o.on('--archive PATH') { |v| options[:archive] = v }
      o.on('--trust-plugins') { options[:trust_plugins] = true }
      o.on('-h', '--help') { puts o; return 0 }
    end
    parser.parse!(argv)
    check(argv.empty? && [:version, :platform, :prefix].all? { |k| options.key?(k) }, parser.to_s)
    result = install(**options)
    puts JSON.generate(result)
    if options[:trust_plugins]
      warn "Set FABRIC_PLUGIN_REGISTRY=#{result['registry'].inspect} before native plugin use."
    else
      warn 'Verified and extracted only; no downloaded code executed. --trust-plugins on a new installation explicitly consents to Core registration.'
    end
    0
  rescue Error, OptionParser::ParseError, SystemCallError, IOError, SocketError, OpenSSL::SSL::SSLError,
      Timeout::Error, URI::InvalidURIError, Net::ProtocolError, Fiddle::DLError => e
    warn "fabric installer: #{e.message}"
    warn 'For offline installation, pass --archive /absolute/path/to/the/pinned/archive.tar.gz; existing prefixes are never reused.'
    1
  end
end

exit FabricDistribution.main(ARGV) if $PROGRAM_NAME == __FILE__
