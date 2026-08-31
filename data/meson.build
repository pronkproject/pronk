pipewire_remote_config = files('pipewire/80-pronk-remotes.conf')
wireplumber_access_configs = files(
  'wireplumber/80-pronk-access.conf',
  'wireplumber/81-pronk-castkms-audio.conf',
)
wireplumber_policy_scripts = files(
  'wireplumber/scripts/pronk-private-policy.lua',
  'wireplumber/scripts/pronk-policy-marker.lua',
  'wireplumber/scripts/pronk-cast-audio-default.lua',
)
backend_mock_socket = files('systemd/user/pronk-backend-mock.socket')
backend_mock_definition = files('backends.d/mock.toml')
chromiacast_socket = files('systemd/user/pronk-chromiacast.socket')
chromiacast_definition = files('backends.d/chromiacast.toml')
public_interface = files('dbus-1/interfaces/io.github.pronkproject.Pronk1.xml')

systemd_dep = dependency('systemd')
systemd_user_unit_dir = systemd_dep.get_variable(pkgconfig: 'systemduserunitdir')
backend_unit_configuration = configuration_data()
backend_unit_configuration.set('libexecdir', pronk_backend_install_dir)
backend_mock_service = configure_file(
  input: 'systemd/user/pronk-backend-mock@.service.in',
  output: 'pronk-backend-mock@.service',
  configuration: backend_unit_configuration,
  install: true,
  install_dir: systemd_user_unit_dir,
)
chromiacast_service = configure_file(
  input: 'systemd/user/pronk-chromiacast@.service.in',
  output: 'pronk-chromiacast@.service',
  configuration: backend_unit_configuration,
  install: true,
  install_dir: systemd_user_unit_dir,
)

core_configuration = configuration_data()
core_configuration.set('bindir', pronk_bindir)
core_service = configure_file(
  input: 'systemd/user/pronk.service.in',
  output: 'pronk.service',
  configuration: core_configuration,
  install: true,
  install_dir: systemd_user_unit_dir,
)
dbus_service = configure_file(
  input: 'dbus-1/services/io.github.pronkproject.Pronk1.service.in',
  output: 'io.github.pronkproject.Pronk1.service',
  configuration: core_configuration,
  install: true,
  install_dir: join_paths(get_option('datadir'), 'dbus-1', 'services'),
)

install_data(
  backend_mock_socket,
  install_dir: systemd_user_unit_dir,
)

install_data(
  chromiacast_socket,
  install_dir: systemd_user_unit_dir,
)

install_data(
  backend_mock_definition,
  install_dir: join_paths(get_option('prefix'), 'lib', 'pronk', 'backends.d'),
)

install_data(
  chromiacast_definition,
  install_dir: join_paths(get_option('prefix'), 'lib', 'pronk', 'backends.d'),
)

install_data(
  public_interface,
  install_dir: join_paths(get_option('datadir'), 'dbus-1', 'interfaces'),
)

install_data(
  pipewire_remote_config,
  install_dir: join_paths(
    get_option('datadir'),
    'pipewire',
    'pipewire.conf.d',
  ),
)

install_data(
  wireplumber_access_configs,
  install_dir: join_paths(
    get_option('datadir'),
    'wireplumber',
    'wireplumber.conf.d',
  ),
)

install_data(
  wireplumber_policy_scripts,
  install_dir: join_paths(
    get_option('datadir'),
    'wireplumber',
    'scripts',
  ),
)

test(
  'public-interface-xml',
  xmllint,
  args: ['--noout', public_interface],
)

systemd_analyze = find_program('systemd-analyze')
test(
  'backend-systemd-unit-contract',
  sh,
  args: [
    files('../tests/backend-activation/check-systemd-units'),
    systemd_analyze.full_path(),
    backend_mock_socket,
    backend_mock_service,
    pronk_backend_install_dir,
    'pronk-backend-mock',
    'pronk/backends/mock.sock',
    'pronk-backend-mock',
    'isolated',
  ],
)

test(
  'chromiacast-systemd-unit-contract',
  sh,
  args: [
    files('../tests/backend-activation/check-systemd-units'),
    systemd_analyze.full_path(),
    chromiacast_socket,
    chromiacast_service,
    pronk_backend_install_dir,
    'pronk-chromiacast',
    'pronk/backends/chromiacast.sock',
    'pronk-chromiacast',
    'networked',
  ],
)

test(
  'core-control-plane-contract',
  sh,
  args: [
    files('../tests/backend-activation/check-core-control-plane'),
    systemd_analyze.full_path(),
    core_service,
    dbus_service,
    public_interface,
    files('../crates/pronk/Cargo.toml'),
    pronk_bindir,
  ],
)

test(
  'backend-registry-contract',
  sh,
  args: [
    files('../tests/backend-activation/check-backend-definition'),
    backend_mock_definition,
    'mock',
    'pronk/backends/mock.sock',
    'pronk-backend-mock.socket',
    'pronk-backend-mock@.service',
  ],
)

test(
  'chromiacast-registry-contract',
  sh,
  args: [
    files('../tests/backend-activation/check-backend-definition'),
    chromiacast_definition,
    'chromiacast',
    'pronk/backends/chromiacast.sock',
    'pronk-chromiacast.socket',
    'pronk-chromiacast@.service',
  ],
)

pw_config = find_program('pw-config')
pipewire_daemon = find_program('pipewire')
pw_cli = find_program('pw-cli')
wpctl = find_program('wpctl')
wireplumber_daemon = find_program('wireplumber')
timeout_program = find_program('timeout')
gst_launch = find_program('gst-launch-1.0')
pw_dump = find_program('pw-dump')
jq = find_program('jq')

test(
  'pipewire-remote-config-contract',
  sh,
  args: [
    files('../tests/pipewire-remote/check-pipewire-config'),
    pw_config.full_path(),
    pipewire_remote_config,
  ],
)

test(
  'pipewire-remote-classification',
  sh,
  args: [
    files('../tests/pipewire-remote/check-pipewire-classification'),
    pipewire_daemon.full_path(),
    pw_cli.full_path(),
    pipewire_remote_config,
  ],
  timeout: 30,
)

test(
  'wireplumber-private-media-policy',
  sh,
  args: [
    files('../tests/pipewire-remote/check-wireplumber-access'),
    pipewire_daemon.full_path(),
    wireplumber_daemon.full_path(),
    pw_cli.full_path(),
    wpctl.full_path(),
    timeout_program.full_path(),
    gst_launch.full_path(),
    pw_dump.full_path(),
    jq.full_path(),
    pipewire_remote_config,
    files('../tests/pipewire-remote/90-pronk-policy-test-remotes.conf'),
    wireplumber_access_configs[0],
    wireplumber_access_configs[1],
    wireplumber_policy_scripts[0],
    wireplumber_policy_scripts[1],
    wireplumber_policy_scripts[2],
    join_paths(cargo_target_dir, cargo_profile_dir, 'pronk-pipewire-policy-gate-test'),
  ],
  depends: cargo_workspace,
  timeout: 30,
)

test(
  'backend-gstreamer-media-session',
  sh,
  args: [
    files('../tests/backend-activation/run-media-session-gate'),
    pipewire_daemon.full_path(),
    wireplumber_daemon.full_path(),
    pw_cli.full_path(),
    timeout_program.full_path(),
    gst_launch.full_path(),
    pw_dump.full_path(),
    join_paths(cargo_target_dir, cargo_profile_dir, 'pronk-backend-p2p-test'),
    systemd_socket_activate.full_path(),
    join_paths(cargo_target_dir, cargo_profile_dir, 'pronk-backend-mock'),
    pipewire_remote_config,
    wireplumber_access_configs[0],
    wireplumber_policy_scripts[0],
    wireplumber_policy_scripts[1],
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  suite: 'integration',
  timeout: 90,
)

test(
  'chromiacast-gstreamer-media-session',
  sh,
  args: [
    files('../tests/backend-activation/run-media-session-gate'),
    pipewire_daemon.full_path(),
    wireplumber_daemon.full_path(),
    pw_cli.full_path(),
    timeout_program.full_path(),
    gst_launch.full_path(),
    pw_dump.full_path(),
    join_paths(cargo_target_dir, cargo_profile_dir, 'pronk-chromiacast-p2p-test'),
    systemd_socket_activate.full_path(),
    join_paths(cargo_target_dir, cargo_profile_dir, 'pronk-chromiacast'),
    pipewire_remote_config,
    wireplumber_access_configs[0],
    wireplumber_policy_scripts[0],
    wireplumber_policy_scripts[1],
    edid_decode.full_path(),
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  timeout: 90,
)
