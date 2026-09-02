helper_version = meson.project_version().split('.')
helper_config = configuration_data()
helper_config.set('PRONK_BUILD_MAJOR', helper_version[0].to_int())
helper_config.set('PRONK_BUILD_MINOR', helper_version[1].to_int())
helper_config.set('PRONK_BUILD_PATCH', helper_version[2].to_int())
helper_config.set_quoted('PRONKD_PATH', join_paths(pronk_bindir, 'pronkd'))
helper_config.set_quoted('PRONKD_SYSTEM_UNIT', 'pronk.service')
helper_config.set_quoted('PRONK_SERVICE_USER', 'pronk')
configure_file(
  output: 'config.h',
  configuration: helper_config,
)

libdrm_dependency = dependency('libdrm', version: '>= 2.4.120')
libsystemd_dependency = dependency('libsystemd', version: '>= 255')

helper_protocol = static_library(
  'pronk-helper-protocol',
  'protocol.c',
  install: false,
)

helper_protocol_test = executable(
  'pronk-helper-protocol-test',
  'test-protocol.c',
  link_with: helper_protocol,
  install: false,
)
test('helper-protocol-c', helper_protocol_test)

helper_caller_test = executable(
  'pronk-helper-caller-test',
  ['test-caller.c', 'caller.c'],
  dependencies: libsystemd_dependency,
  install: false,
)
test('helper-caller-c', helper_caller_test)

helper_transport_test = executable(
  'pronk-helper-transport-test',
  ['test-transport.c', 'transport.c'],
  install: false,
)
test('helper-transport-c', helper_transport_test)

executable(
  'pronk-grant-helper',
  [
    'main.c',
    'caller.c',
    'device.c',
    'grant.c',
    'transport.c',
  ],
  dependencies: [libdrm_dependency, libsystemd_dependency],
  c_args: ['-fstack-protector-strong'],
  link_args: ['-Wl,-z,relro,-z,now'],
  link_with: helper_protocol,
  pie: true,
  install: true,
  install_dir: pronk_libexec_dir,
)
