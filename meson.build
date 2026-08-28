project(
  'pronk',
  'c',
  version: '0.1.0',
  meson_version: '>= 1.4.0',
  default_options: [
    'c_std=gnu11',
    'prefix=/usr',
    'warning_level=3',
    'werror=true',
  ],
)

pronk_libexec_dir = get_option('libexecdir')
if not pronk_libexec_dir.startswith('/')
  pronk_libexec_dir = join_paths(
    get_option('prefix'),
    pronk_libexec_dir,
  )
endif
pronk_backend_install_dir = join_paths(pronk_libexec_dir, 'pronk')
pronk_bindir = get_option('bindir')
if not pronk_bindir.startswith('/')
  pronk_bindir = join_paths(get_option('prefix'), pronk_bindir)
endif
cargo = find_program('cargo')
sh = find_program('sh')
xmllint = find_program('xmllint')
cargo_target_dir = join_paths(meson.project_build_root(), 'cargo-target')

cargo_workspace = custom_target(
  'cargo-workspace',
  output: 'cargo-workspace.stamp',
  command: [
    sh,
    files('build-aux/cargo-build'),
    cargo,
    meson.project_source_root(),
    cargo_target_dir,
    '@OUTPUT@',
  ],
  build_always_stale: true,
  build_by_default: true,
  console: true,
)

pronkd = custom_target(
  'pronkd',
  output: 'pronkd',
  command: [
    sh,
    files('build-aux/copy-executable'),
    join_paths(cargo_target_dir, 'debug', 'pronkd'),
    '@OUTPUT@',
  ],
  depends: cargo_workspace,
  build_by_default: true,
  install: true,
  install_dir: pronk_bindir,
)

pronkctl = custom_target(
  'pronkctl',
  output: 'pronkctl',
  command: [
    sh,
    files('build-aux/copy-executable'),
    join_paths(cargo_target_dir, 'debug', 'pronkctl'),
    '@OUTPUT@',
  ],
  depends: cargo_workspace,
  build_by_default: true,
  install: true,
  install_dir: pronk_bindir,
)

pronk_backend_mock = custom_target(
  'pronk-backend-mock',
  output: 'pronk-backend-mock',
  command: [
    sh,
    files('build-aux/copy-executable'),
    join_paths(cargo_target_dir, 'debug', 'pronk-backend-mock'),
    '@OUTPUT@',
  ],
  depends: cargo_workspace,
  build_by_default: true,
  install: true,
  install_dir: pronk_backend_install_dir,
)

pronk_chromiacast = custom_target(
  'pronk-chromiacast',
  output: 'pronk-chromiacast',
  command: [
    sh,
    files('build-aux/copy-executable'),
    join_paths(cargo_target_dir, 'debug', 'pronk-chromiacast'),
    '@OUTPUT@',
  ],
  depends: cargo_workspace,
  build_by_default: true,
  install: true,
  install_dir: pronk_backend_install_dir,
)

run_target(
  'cargo-test',
  command: [
    cargo,
    'test',
    '--workspace',
    '--locked',
    '--target-dir', cargo_target_dir,
    '--manifest-path', join_paths(meson.project_source_root(), 'Cargo.toml'),
  ],
)

subdir('tests/castkms-live')
subdir('tests/backend-activation')
subdir('data')
