system_backend_mock_socket = files('pronk-backend-mock.socket')
system_chromiacast_socket = files('pronk-chromiacast.socket')
system_pipewire_socket = files('pronk-pipewire.socket')

system_backend_unit_configuration = configuration_data()
system_backend_unit_configuration.set('libexecdir', pronk_backend_install_dir)
system_backend_mock_service = configure_file(
  input: 'pronk-backend-mock@.service.in',
  output: 'pronk-backend-mock@.service',
  configuration: system_backend_unit_configuration,
  install: true,
  install_dir: systemd_system_unit_dir,
)
system_chromiacast_service = configure_file(
  input: 'pronk-chromiacast@.service.in',
  output: 'pronk-chromiacast@.service',
  configuration: system_backend_unit_configuration,
  install: true,
  install_dir: systemd_system_unit_dir,
)

system_core_configuration = configuration_data()
system_core_configuration.set('bindir', pronk_bindir)
system_core_service = configure_file(
  input: 'pronk.service.in',
  output: 'pronk.service',
  configuration: system_core_configuration,
  install: true,
  install_dir: systemd_system_unit_dir,
)

system_media_unit_configuration = configuration_data()
system_media_unit_configuration.set(
  'pipewire',
  'pipewire',
)
system_media_unit_configuration.set(
  'wireplumber',
  'wireplumber',
)
system_pipewire_service = configure_file(
  input: 'pronk-pipewire.service.in',
  output: 'pronk-pipewire.service',
  configuration: system_media_unit_configuration,
  install: true,
  install_dir: systemd_system_unit_dir,
)
system_wireplumber_service = configure_file(
  input: 'pronk-wireplumber.service.in',
  output: 'pronk-wireplumber.service',
  configuration: system_media_unit_configuration,
  install: true,
  install_dir: systemd_system_unit_dir,
)
system_dbus_service = configure_file(
  input: '../../dbus-1/system-services/io.github.pronkproject.Pronk1.service.in',
  output: 'io.github.pronkproject.Pronk1.service',
  configuration: system_core_configuration,
  install: true,
  install_dir: join_paths(get_option('datadir'), 'dbus-1', 'system-services'),
)

install_data(
  system_backend_mock_socket,
  install_dir: systemd_system_unit_dir,
)

install_data(
  system_chromiacast_socket,
  install_dir: systemd_system_unit_dir,
)

install_data(
  system_pipewire_socket,
  install_dir: systemd_system_unit_dir,
)
