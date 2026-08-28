systemd_socket_activate = find_program('systemd-socket-activate')
edid_decode = find_program('edid-decode')
env_program = find_program('env')
activation_probe = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-systemd-activation-probe',
)
activation_test = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-systemd-activation-test',
)
backend_p2p_test = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-backend-p2p-test',
)
chromiacast_p2p_test = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-chromiacast-p2p-test',
)
edid_upstream_test = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-edid-upstream-test',
)
caller_identity_test = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-caller-identity-test',
)
mock_backend = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-backend-mock',
)
chromiacast_backend = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-chromiacast',
)
public_inventory_test_service = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-public-inventory-test-service',
)
pronkctl_test = join_paths(
  cargo_target_dir,
  'debug',
  'pronkctl',
)
public_lifecycle_test_client = join_paths(
  cargo_target_dir,
  'debug',
  'pronk-public-lifecycle-test-client',
)

test(
  'backend-systemd-activation',
  env_program,
  args: [
    activation_test,
    systemd_socket_activate.full_path(),
    activation_probe,
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  timeout: 30,
)

test(
  'edid-upstream-conformance',
  env_program,
  args: [
    edid_upstream_test,
    edid_decode.full_path(),
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  timeout: 30,
)

test(
  'caller-session-identity',
  env_program,
  args: [
    caller_identity_test,
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  timeout: 15,
)

test(
  'backend-systemd-p2p-protocol',
  env_program,
  args: [
    backend_p2p_test,
    systemd_socket_activate.full_path(),
    mock_backend,
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  timeout: 45,
)

test(
  'chromiacast-systemd-p2p-protocol',
  env_program,
  args: [
    chromiacast_p2p_test,
    systemd_socket_activate.full_path(),
    chromiacast_backend,
    edid_decode.full_path(),
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  timeout: 30,
)

busctl = find_program('busctl')
test(
  'public-device-inventory',
  sh,
  args: [
    files('run-public-inventory-gate'),
    busctl.full_path(),
    systemd_socket_activate.full_path(),
    mock_backend,
    public_inventory_test_service,
    pronkctl_test,
    public_lifecycle_test_client,
  ],
  depends: cargo_workspace,
  workdir: meson.project_source_root(),
  timeout: 30,
)
