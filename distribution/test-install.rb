#!/usr/bin/ruby
# Run explicitly: /usr/bin/ruby distribution/test-install.rb /absolute/path/to/fabric-v0.1.2-macos-arm64.tar.gz
# Uses the real public payload and real Core; never downloads or uses the caller's home/registry.
require_relative 'install'

module InstallerCheck
  module_function

  def assert(condition, message)
    raise message unless condition
  end

  def rejects(fragment)
    begin
      yield
    rescue FabricDistribution::Error => e
      assert(e.message.include?(fragment), "expected #{fragment.inspect}, got #{e.message.inspect}")
      return
    end
    raise "expected rejection: #{fragment}"
  end

  def gzip(bytes)
    io = StringIO.new(''.b)
    writer = Zlib::GzipWriter.new(io)
    writer.mtime = 0
    writer.write(bytes)
    writer.close
    io.string
  end

  def header(name, type = '0', size = 0, link = '')
    h = "\0" * 512
    h[0, name.bytesize] = name
    h[100, 8] = "0000700\0"
    h[108, 8] = "0000000\0"
    h[116, 8] = "0000000\0"
    h[124, 12] = "%011o\0" % size
    h[136, 12] = "00000000000\0"
    h[148, 8] = ' ' * 8
    h[156, 1] = type
    h[157, link.bytesize] = link
    h[257, 8] = "ustar\00000"
    h[148, 8] = "%06o\0 " % h.bytes.inject(0, :+)
    h
  end

  def archive(entries)
    tar = ''.b
    entries.each do |name, type, bytes, link|
      bytes ||= ''.b
      tar << header(name, type || '0', bytes.bytesize, link || '') << bytes
      tar << "\0" * ((512 - bytes.bytesize % 512) % 512)
    end
    gzip(tar + "\0" * 1024)
  end

  def changed_manifest(files, inventory)
    copy = files.dup
    manifest = JSON.parse(inventory)
    yield copy, manifest
    updated = JSON.pretty_generate(manifest) + "\n"
    copy['release.json'] = updated
    [copy, updated, manifest]
  end

  def run_cli(env, *args)
    stdout, stderr, status = Open3.capture3(env, '/usr/bin/ruby', File.join(__dir__, 'install.rb'),
      *args, :chdir => '/', :unsetenv_others => true)
    warn JSON.generate('command' => ['/usr/bin/ruby', File.join(__dir__, 'install.rb')] + args, 'cwd' => '/', 'exit' => status.exitstatus)
    warn "stdout:\n#{stdout}\nstderr:\n#{stderr}"
    [stdout, stderr, status]
  end

  def main(argv)
    assert(argv.length == 1 && argv[0].start_with?('/'), 'usage: /usr/bin/ruby distribution/test-install.rb ABS_PUBLIC_TAR_GZ')
    d = FabricDistribution
    inventory, manifest, pin = d.pins
    bytes = d.read_file(argv[0], d::MAX_ARCHIVE)
    files = d.verify(bytes, inventory, manifest, pin)
    assert(files.size == 18, 'actual release must contain exactly 17 payloads and release.json')
    assert(d.content_hash('b' => 'two', 'a' => 'one') == d.hash("a\0#{d.hash('one')}\nb\0#{d.hash('two')}\n"), 'SDK sorted filename-NUL-digest-LF algorithm')

    rejects('archive checksum mismatch') { d.verify(bytes + 'corrupt', inventory, manifest, pin) }
    rejects('archive checksum mismatch') { d.verify('not a gzip archive', inventory, manifest, pin) }
    rejects('unsupported version') { d.install(:version => '0.1.3', :platform => d::PLATFORM, :prefix => '/unused') }
    rejects('unsupported platform') { d.install(:version => d::VERSION, :platform => 'linux-x64', :prefix => '/unused') }
    %w[version platform source_sha build_kind archive cli_version source_url].each do |key|
      args = changed_manifest(files, inventory) { |_f, m| m[key] = 'wrong' }
      rejects("release provenance mismatch: #{key}") { d.verify_payload(*args) }
    end
    args = changed_manifest(files, inventory) { |_f, m| m['build']['cargo_lock_sha256'] = '0' * 64 }
    rejects('release build provenance') { d.verify_payload(*args) }
    args = changed_manifest(files, inventory) { |_f, m| m['files'].delete('sdk/index.mjs') }
    rejects('incomplete release inventory') { d.verify_payload(*args) }
    %w[id version protocol sha256].each do |key|
      args = changed_manifest(files, inventory) { |_f, m| m['plugins'][0][key] = 'wrong' }
      rejects(key == 'id' ? 'official plugin IDs' : 'official plugin version/protocol/pin') { d.verify_payload(*args) }
    end
    args = changed_manifest(files, inventory) { |_f, m| m['plugins'][0]['sha256'] = '0' * 64 }
    rejects('plugin content-tree checksum mismatch') { d.verify_payload(*args) }
    args = changed_manifest(files, inventory) { |_f, m| m['omp']['sha256'] = '0' * 64 }
    rejects('OMP release pin') { d.verify_payload(*args) }
    args = changed_manifest(files, inventory) { |_f, m| m['omp_extension']['sha256'] = '0' * 64 }
    rejects('OMP extension pin') { d.verify_payload(*args) }
    args = changed_manifest(files, inventory) { |f, _m| f['integrations/omp/fabric.ts'] += '// corruption' }
    rejects('payload checksum mismatch: integrations/omp/fabric.ts') { d.verify_payload(*args) }
    altered = files.merge('bin/fabric' => 'not the real binary')
    rejects('payload checksum mismatch: bin/fabric') { d.verify_payload(altered, inventory, manifest) }
    rejects('archive file set') { d.verify_payload(files.reject { |name, _| name == 'sdk/index.mjs' }, inventory, manifest) }
    rejects('archive file set') { d.verify_payload(files.merge('unreviewed' => ''), inventory, manifest) }
    rejects('embedded release.json') { d.verify_payload(files.merge('release.json' => '{}'), inventory, manifest) }

    # Re-pin deliberately modified fixtures to exercise semantic metadata validation,
    # not merely the outer archive or file checksum guard.
    %w[omp codex local].each do |id|
      %w[protocol runtime args native sdk entry id kind].each do |key|
        args = changed_manifest(files, inventory) do |f, m|
          package_path = "plugins/#{id}/package.json"
          package = JSON.parse(f.fetch(package_path))
          package['fabricPlugin'][key] = 'wrong'
          f[package_path] = JSON.generate(package)
          m['files'][package_path] = d.hash(f[package_path])
          plugin = (m['plugins'] + m['target_plugins']).find { |p| p['id'] == id }
          plugin['sha256'] = d.content_hash('package.json' => f[package_path], 'plugin' => f.fetch("plugins/#{id}/plugin"))
        end
        rejects("official plugin metadata mismatch: #{id}") { d.verify_payload(*args) }
      end
    end

    root = d::ROOT
    valid_entries = [[root + '/', '5', '']] + files.map { |name, data| ["#{root}/#{name}", '0', data] }
    assert(d.parse_tar(archive(valid_entries)) == files, 'isolated ustar round-trip')
    [['../escape', 'unsafe tar path'], ['/absolute', 'unsafe tar path'],
      [root + '/../escape', 'unsafe tar path'], [root + '/./bin/fabric', 'unsafe tar path'],
      [root + '//bin/fabric', 'unsafe tar path'], [root + '/bin\\fabric', 'unsafe tar path'],
      [root + '/unreviewed', 'unpinned archive file']].each do |name, error|
      rejects(error) { d.parse_tar(archive([[name, '0', '']])) }
    end
    rejects('unpinned archive directory') { d.parse_tar(archive([[root + '/unreviewed/', '5', '']])) }
    %w[1 2 3 4 6 x g L K].each do |type|
      rejects('unsafe tar entry type') { d.parse_tar(archive([[root + '/bin/fabric', type, '', '../escape']])) }
    end
    rejects('unsafe tar link target') { d.parse_tar(archive([[root + '/bin/fabric', '0', '', '../escape']])) }
    rejects('duplicate or case-alias') { d.parse_tar(archive([[root + '/', '5', ''], [root + '/', '5', '']])) }
    rejects('duplicate or case-alias') do
      d.parse_tar(archive([[root + '/bin/fabric', '0', ''], [root + '/BIN/FABRIC', '0', '']]))
    end
    damaged = header(root + '/', '5')
    damaged.setbyte(0, damaged.getbyte(0) ^ 1)
    rejects('tar header checksum') { d.parse_tar(gzip(damaged + "\0" * 1024)) }
    rejects('truncated tar') { d.parse_tar(gzip(header(root + '/bin/fabric', '0', 100))) }
    rejects('invalid tar end marker') { d.parse_tar(gzip("\0" * 512 + 'X' * 512)) }
    rejects('unsafe trailing tar data') { d.parse_tar(gzip("\0" * 1024 + 'X' * 512)) }
    rejects('trailing or concatenated gzip data') { d.parse_tar(archive([]) + 'trailer') }
    rejects('invalid tar entry size') { d.parse_tar(gzip(header(root + '/bin/fabric', '0', d::MAX_ARCHIVE + 1))) }

    temp_parent = File.realpath(Dir.tmpdir)
    Dir.mktmpdir('fabric-installer-check-', temp_parent) do |fixture|
      home = File.join(fixture, 'home')
      Dir.mkdir(home, 0700)
      env = { 'PATH' => '/usr/bin:/bin', 'HOME' => home, 'TMPDIR' => fixture,
        'XDG_DATA_HOME' => File.join(fixture, 'data'), 'XDG_CONFIG_HOME' => File.join(fixture, 'config'),
        'PI_CODING_AGENT_DIR' => File.join(fixture, 'omp'), 'CODEX_HOME' => File.join(fixture, 'codex'),
        'FABRIC_PLUGIN_REGISTRY' => File.join(fixture, 'never-use-this-registry') }
      selectors = ['--version', d::VERSION, '--platform', d::PLATFORM, '--archive', argv[0]]
      destination = File.join(fixture, 'install')
      stdout, stderr, status = run_cli(env, *selectors, '--prefix', destination)
      assert(status.success?, "real default install failed: #{stderr}")
      result = JSON.parse(stdout)
      assert(result['plugins_registered'] == false && result['registry'].nil?, 'default install must not register plugins')
      assert(!File.exist?(File.join(destination, 'registry')), 'default install created a registry')
      installed = Dir.glob(File.join(destination, '**', '*')).select { |p| File.file?(p) }
      assert(installed.size == 18 && d.hash(File.binread(File.join(destination, 'bin/fabric'))) == manifest['files']['bin/fabric'], 'default install must contain exactly the real release payload')

      trusted = File.join(fixture, 'trusted')
      stdout, stderr, status = run_cli(env, *selectors, '--prefix', trusted, '--trust-plugins')
      assert(status.success?, "real Core registration failed: #{stderr}")
      registry = File.join(trusted, 'registry')
      assert(JSON.parse(stdout)['registry'] == registry, 'registration must use the installed private registry')
      assert(!File.exist?(env['FABRIC_PLUGIN_REGISTRY']), 'caller registry environment leaked into installation')
      %w[omp codex local].each do |id|
        output, errors, exit_status = Open3.capture3(env.merge('FABRIC_PLUGIN_REGISTRY' => registry),
          File.join(trusted, 'bin/fabric'), 'plugins', '--registry', registry, 'check', id,
          :chdir => '/', :unsetenv_others => true)
        warn JSON.generate('command' => [File.join(trusted, 'bin/fabric'), 'plugins', '--registry', registry, 'check', id], 'cwd' => '/', 'exit' => exit_status.exitstatus)
        warn "stdout:\n#{output}\nstderr:\n#{errors}"
        if id == 'local'
          assert(exit_status.success? && JSON.parse(output) == { 'compatible' => true, 'healthy' => true }, 'real local target must be healthy')
        else
          # Official native harness CLIs are deliberately not bundled in this release.
          diagnostic = JSON.parse(errors).fetch('error')
          assert(!exit_status.success? && diagnostic['kind'] == 'output' &&
            diagnostic['message'].include?(id) && diagnostic['message'].include?('PATH'),
            "unbundled #{id} must fail with an actionable discovery diagnostic")
        end
      end
      (manifest['plugins'] + manifest['target_plugins']).each do |p|
        content = File.join(registry, 'packages', p['sha256'])
        snapshot = %w[package.json plugin].map { |name| [name, File.binread(File.join(content, name))] }.to_h
        assert(d.content_hash(snapshot) == p['sha256'], 'registered content must use SDK content-addressed hashes')
      end
      collision_paths = [destination, File.join(fixture, 'empty'), File.join(fixture, 'dangling'), File.join(fixture, 'file')]
      Dir.mkdir(collision_paths[1], 0700)
      File.symlink(File.join(fixture, 'missing'), collision_paths[2])
      File.write(collision_paths[3], 'preserve me')
      collision_paths.each do |path|
        rejects('destination already exists') { d.validate_prefix(path) }
        stage = Dir.mktmpdir('.race-', fixture)
        begin
          rejects('destination publication refused') { d.publish(stage, path) }
          assert(File.directory?(stage), 'failed publication must not move source or replace destination')
        ensure
          Dir.rmdir(stage)
        end
      end
      assert(File.symlink?(collision_paths[2]) && File.readlink(collision_paths[2]) == File.join(fixture, 'missing'), 'dangling symlink was modified')
      assert(File.read(collision_paths[3]) == 'preserve me', 'existing file was modified')
      assert(Dir.empty?(collision_paths[1]), 'existing empty directory was modified')
      rejects('canonical absolute') { d.validate_prefix(File.join(fixture, '.', 'noncanonical')) }
      link = File.join(fixture, 'parent-link')
      File.symlink(home, link)
      rejects('parent must already exist and be canonical') { d.validate_prefix(File.join(link, 'install')) }
      unsafe = File.join(fixture, 'unsafe-parent')
      Dir.mkdir(unsafe, 0700)
      safe_child = File.join(unsafe, 'private-child')
      Dir.mkdir(safe_child, 0700)
      [0720, 0702, 0777].each do |mode|
        File.chmod(mode, unsafe)
        [unsafe, safe_child].each do |parent|
          prefix = File.join(parent, 'install')
          rejects('installation ancestry') { d.install(version: d::VERSION, platform: d::PLATFORM,
            prefix: prefix, archive: argv[0], trust_plugins: true) }
          assert(!File.exist?(prefix) && Dir.glob(File.join(parent, '.fabric-install-*')).empty?,
            'hostile ancestry must fail before staging, publication or trusted execution')
        end
      end
      File.chmod(01777, unsafe)
      assert(d.validate_prefix(File.join(unsafe, 'sticky-install')) == unsafe, 'owner/root sticky parents protect staging entries')
      File.chmod(0700, unsafe)
      corrupt = File.join(fixture, 'corrupt.tar.gz')
      File.binwrite(corrupt, bytes + 'corrupt')
      bad_destination = File.join(fixture, 'corrupt-install')
      _stdout, stderr, status = run_cli(env, '--version', d::VERSION, '--platform', d::PLATFORM,
        '--archive', corrupt, '--prefix', bad_destination)
      assert(!status.success? && stderr.include?('archive checksum mismatch'), 'CLI must reject corrupt archive before parsing')
      assert(!File.exist?(bad_destination) && Dir.glob(File.join(fixture, '.fabric-install-*')).empty?, 'corrupt input left an installation or staging data')
      _stdout, stderr, status = run_cli(env, *selectors, '--prefix', destination)
      assert(!status.success? && stderr.include?('destination already exists'), 'CLI must not reuse existing installations')
    end
    puts 'PASS: pinned public archive, isolated default install, real Core registration, provenance/inventory/plugin/extension guards, unsafe tar rejection, no-clobber collisions'
  end
end

InstallerCheck.main(ARGV) if $PROGRAM_NAME == __FILE__
